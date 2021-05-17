use core::borrow::BorrowMut;
use heapless::{
    spsc::{Consumer, Producer, Queue},
    Vec,
};
use serde::Serialize;
use smoltcp_nal::embedded_nal::{Mode, SocketAddr, TcpStack};

use super::NetworkReference;
use crate::hardware::design_parameters::SAMPLE_BUFFER_SIZE;

// The number of data blocks that we will buffer in the queue.
type BlockBufferSize = heapless::consts::U10;

pub fn setup_streaming(
    stack: NetworkReference,
) -> (BlockGenerator, DataStream) {
    let queue = cortex_m::singleton!(: Queue<AdcDacData, BlockBufferSize> = Queue::new()).unwrap();

    let (producer, consumer) = queue.split();

    let generator = BlockGenerator::new(producer);

    let stream = DataStream::new(stack, consumer);

    (generator, stream)
}

pub struct AdcDacData {
    block_id: u32,
    adcs: [[u16; SAMPLE_BUFFER_SIZE]; 2],
    dacs: [[u16; SAMPLE_BUFFER_SIZE]; 2],
}

pub struct BlockGenerator {
    queue: Producer<'static, AdcDacData, BlockBufferSize>,
    current_id: u32,
}

impl BlockGenerator {
    pub fn new(queue: Producer<'static, AdcDacData, BlockBufferSize>) -> Self {
        Self {
            queue,
            current_id: 0,
        }
    }

    pub fn send(
        &mut self,
        adcs: &[&[u16; SAMPLE_BUFFER_SIZE]; 2],
        dacs: &[&mut [u16; SAMPLE_BUFFER_SIZE]; 2],
    ) {
        let block = AdcDacData {
            block_id: self.current_id,
            adcs: [*adcs[0], *adcs[1]],
            dacs: [*dacs[0], *dacs[1]],
        };

        self.current_id = self.current_id.wrapping_add(1);

        // We perform best-effort enqueueing of the data block.
        self.queue.enqueue(block).ok();
    }
}

pub struct DataStream {
    stack: NetworkReference,
    socket: Option<<NetworkReference as TcpStack>::TcpSocket>,
    queue: Consumer<'static, AdcDacData, BlockBufferSize>,
    remote: Option<SocketAddr>,
}

struct DataBlock {
    block_id: u32,
    block_size: usize,
    adcs: [[u16; SAMPLE_BUFFER_SIZE]; 2],
    dacs: [[u16; SAMPLE_BUFFER_SIZE]; 2],
}

impl DataBlock {
    pub fn serialize<T: heapless::ArrayLength<u8>>(self) -> Vec<u8, T> {
        let mut vec: Vec<u8, T> = Vec::new();
        vec.extend_from_slice(&self.block_id.to_be_bytes()).unwrap();
        vec.extend_from_slice(&self.block_size.to_be_bytes()).unwrap();
        for device in &[self.adcs, self.dacs] {
            for channel in device {
                for sample in channel {
                    vec.extend_from_slice(&sample.to_be_bytes()).unwrap();
                }
            }
        }

        vec
    }

}

impl DataStream {
    pub fn new(
        stack: NetworkReference,
        consumer: Consumer<'static, AdcDacData, BlockBufferSize>,
    ) -> Self {
        Self {
            stack,
            socket: None,
            remote: None,
            queue: consumer,
        }
    }

    fn close(&mut self) {
        // Note(unwrap): We guarantee that the socket is available above.
        let socket = self.socket.take().unwrap();
        self.stack.close(socket).unwrap();

        log::info!("Stream Disconnecting");
    }

    fn open(&mut self, remote: SocketAddr) -> Result<(), ()> {
        if self.socket.is_some() {
            self.close();
        }

        let socket =
            self.stack
                .open(Mode::NonBlocking)
                .map_err(|err| match err {
                    <NetworkReference as TcpStack>::Error::NoIpAddress => (),
                    _ => ()
                })?;

        // TODO: How should we handle a connection failure?
        let socket = self.stack.connect(socket, remote).unwrap();

        log::info!("Stream connecting to {:?}", remote);

        // Note(unwrap): The socket will be empty before we replace it.
        self.socket.replace(socket);

        Ok(())
    }

    pub fn set_remote(&mut self, remote: SocketAddr) {
        // If the remote is identical to what we already have, do nothing.
        if let Some(current_remote) = self.remote {
            if current_remote == remote {
                return;
            }
        }

        // Open the new remote connection.
        self.open(remote).ok();
        self.remote = Some(remote);
    }

    fn manage_reconnection(&mut self) {
        if self.socket.is_none() || self.remote.is_none() {
            return
        }

        let mut socket = self.socket.borrow_mut().unwrap();
        let connected = match self.stack.is_connected(&mut socket) {
            Ok(connected) => connected,
            _ => return,
        };

        if !connected {
            self.socket.replace(self.stack.connect(socket, self.remote.unwrap()).unwrap());
        }
    }

    pub fn process(&mut self) {
        if let Some(data) = self.queue.dequeue() {

            // If there's no socket available, try to connect to our remote.
            if self.socket.is_none() && self.remote.is_some() {
                // If we still can't open the remote, continue.
                if self.open(self.remote.unwrap()).is_err() {
                    return;
                }
            }

            // Reconnect the socket if we're no longer connected.
            self.manage_reconnection();

            let block = DataBlock {
                adcs: data.adcs,
                dacs: data.dacs,
                block_id: data.block_id,
                block_size: SAMPLE_BUFFER_SIZE,
            };

            // Serialize the datablock.
            // TODO: Do we want to packetize the data block as well?
            let data: Vec<u8, heapless::consts::U256> = block.serialize();

            let mut socket = self.socket.borrow_mut().unwrap();

            // Transmit the data block.
            // TODO: How should we handle partial packet transmission?
            // TODO: Should we measure how many packets get dropped as telemetry?
            match self.stack.write(&mut socket, &data) {
                Ok(len) => {
                    if len != data.len() {
                        log::warn!("Short message: {} {}", len, data.len());
                        //self.close();
                    }
                },
                _ => {},
            }
        }
    }
}

#!/usr/bin/python3
"""
Authors:
    Kai A. Müller, Deutsches Zentrum für Luft- und Raumfahrt

Description: Setting PID Parameters for the Stabilizer
"""
import argparse
import asyncio
import collections
import logging

from math import pi, inf

import miniconf

import stabilizer

logger = logging.getLogger(__name__)

Argument = collections.namedtuple("Argument", ["positionals", "keywords"])


def add_argument(*args, **kwargs):
    """ Convert arguments into an Argument tuple. """
    return Argument(args, kwargs)
Filter = collections.namedtuple(
    "Filter", ["help", "arguments", "coefficients"])


def _main():
    parser = argparse.ArgumentParser(
        description="Configure Stabilizer dual-pid  parameters."
                    "Note: This script assumes an AFE input gain of 1.")
    parser.add_argument('-v', '--verbose', action='count', default=0,
                        help='Increase logging verbosity')
    parser.add_argument("--broker", "-b", type=str, default="mqtt",
                        help="The MQTT broker to use to communicate with "
                        "Stabilizer (%(default)s)")
    parser.add_argument("--prefix", "-p", type=str,
                        default="dt/sinara/dual-pid/+",
                        help="The Stabilizer device prefix in MQTT, "
                        "wildcards allowed as long as the match is unique "
                        "(%(default)s)")
    parser.add_argument("--no-discover", "-d", action="store_true",
                        help="Do not discover Stabilizer device prefix.")

    parser.add_argument("--channel", "-c", type=int, choices=[0, 1],
                        required=True, help="The pid channel to configure.")
    parser.add_argument("--sample-period", type=float,
                        default=stabilizer.SAMPLE_PERIOD,
                        help="Sample period in seconds (%(default)s s)")

    parser.add_argument("--x-offset", type=float, default=0,
                        help="The channel input offset (%(default)s V)")
    parser.add_argument("--y-min", type=float,
                        default=-stabilizer.DAC_FULL_SCALE,
                        help="The channel minimum output (%(default)s V)")
    parser.add_argument("--y-max", type=float,
                        default=stabilizer.DAC_FULL_SCALE,
                        help="The channel maximum output (%(default)s V)")
    parser.add_argument("--y-offset", type=float, default=0,
                        help="The channel output offset (%(default)s V)")
    parser.add_argument("--p", default=0, type=float,
                        help="Proportional (P) gain"),
    parser.add_argument("--i", default=0, type=float,
                        help="Integrator (I) gain"),
    parser.add_argument("--d", default=0, type=float,
                        help="Derivative (D) gain"),
    parser.add_argument("--i_limit", default=stabilizer.DAC_FULL_SCALE, type=float,
                        help="Integrator (I) gain limit"),

    args = parser.parse_args()

    logging.basicConfig(
        format='%(asctime)s [%(levelname)s] %(name)s: %(message)s',
        level=logging.WARN - 10*args.verbose)

    if args.no_discover:
        prefix = args.prefix
    else:
        devices = asyncio.run(miniconf.discover(args.broker, args.prefix))
        if not devices:
            raise ValueError("No prefixes discovered.")
        if len(devices) > 1:
            raise ValueError(f"Multiple prefixes discovered ({devices})."
                             "Please specify a more specific --prefix")
        prefix = devices.pop()
        logger.info("Automatically using detected device prefix: %s", prefix)

    async def configure():
        logger.info("Connecting to broker")
        interface = await miniconf.Miniconf.create(prefix, args.broker)

        # Set the coefficients.
        # Note: In the future, we will need to Handle higher-order cascades.
        await interface.set(f"/pid_ch/{args.channel}", {
            "p": args.p,
            "i": args.i,
            "d": args.d,
            "x_offset": stabilizer.voltage_to_machine_units(args.x_offset),
            "y_offset": stabilizer.voltage_to_machine_units(args.y_offset),
            "y_min": stabilizer.voltage_to_machine_units(args.y_min),
            "y_max": stabilizer.voltage_to_machine_units(args.y_max),
            "i_limit": stabilizer.voltage_to_machine_units(args.i_limit),
        })

    asyncio.run(configure())


if __name__ == "__main__":
    _main()

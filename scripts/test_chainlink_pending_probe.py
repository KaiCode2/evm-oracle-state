import sys
import unittest
from pathlib import Path


sys.path.insert(0, str(Path(__file__).resolve().parent))

from chainlink_pending_probe import ExperimentState, decode_chainlink_calldata


TRANSMIT_SELECTOR = bytes.fromhex("b1dc65a4")
TRANSMIT_SECONDARY_SELECTOR = bytes.fromhex("ba0cb29e")
FORWARD_SELECTOR = bytes.fromhex("6fadcf72")
AGGREGATOR = "0x7d4e742018fb52e48b08be73d041c18b21de6fb5"
FORWARDER = "0x7b740d24396b09f992b655a590139d7fbb5c73c8"


def word(value: int) -> bytes:
    return value.to_bytes(32, "big", signed=value < 0)


def dynamic_bytes(value: bytes) -> bytes:
    padding = (-len(value)) % 32
    return word(len(value)) + value + (b"\0" * padding)


def report(timestamp: int, observations: list[int]) -> bytes:
    return b"".join(
        [
            word(timestamp),
            word(0),
            word(4 * 32),
            word(0),
            word(len(observations)),
            *(word(value) for value in observations),
        ]
    )


def transmit_calldata(
    timestamp: int,
    observations: list[int],
    selector: bytes = TRANSMIT_SELECTOR,
) -> str:
    encoded_report = dynamic_bytes(report(timestamp, observations))
    head_size = 7 * 32
    args = b"".join(
        [
            word(1),
            word(2),
            word(3),
            word(head_size),
            word(head_size + len(encoded_report)),
            word(head_size + len(encoded_report) + 32),
            word(0),
        ]
    )
    return "0x" + (selector + args + encoded_report + word(0) + word(0)).hex()


def forward_calldata(target: str, inner_calldata: str) -> str:
    inner = bytes.fromhex(inner_calldata.removeprefix("0x"))
    args = bytes.fromhex(target.removeprefix("0").removeprefix("x").zfill(64))
    args += word(64)
    args += dynamic_bytes(inner)
    return "0x" + (FORWARD_SELECTOR + args).hex()


class ChainlinkPendingProbeTests(unittest.TestCase):
    def test_decodes_direct_transmit_candidate(self) -> None:
        decoded = decode_chainlink_calldata(
            AGGREGATOR,
            transmit_calldata(1_784_106_045, [187_800_000_000, 187_831_000_000, 187_900_000_000]),
        )

        self.assertIsNotNone(decoded)
        self.assertEqual(decoded["path"], "direct_transmit")
        self.assertEqual(decoded["aggregator"], AGGREGATOR)
        self.assertEqual(decoded["observations_timestamp"], 1_784_106_045)
        self.assertEqual(decoded["observation_count"], 3)
        self.assertEqual(decoded["median_answer"], "187831000000")

    def test_decodes_forwarded_transmit_candidate(self) -> None:
        decoded = decode_chainlink_calldata(
            FORWARDER,
            forward_calldata(
                AGGREGATOR,
                transmit_calldata(1_784_106_045, [187_800_000_000, 187_831_000_000, 187_900_000_000]),
            ),
        )

        self.assertIsNotNone(decoded)
        self.assertEqual(decoded["path"], "forward_transmit")
        self.assertEqual(decoded["forwarder"], FORWARDER)
        self.assertEqual(decoded["aggregator"], AGGREGATOR)
        self.assertEqual(decoded["median_answer"], "187831000000")

    def test_decodes_forwarded_svr_candidate(self) -> None:
        decoded = decode_chainlink_calldata(
            FORWARDER,
            forward_calldata(
                AGGREGATOR,
                transmit_calldata(
                    1_784_106_045,
                    [187_800_000_000, 187_831_000_000, 187_900_000_000],
                    selector=TRANSMIT_SECONDARY_SELECTOR,
                ),
            ),
        )

        self.assertIsNotNone(decoded)
        self.assertEqual(decoded["path"], "forward_transmit_secondary")
        self.assertEqual(decoded["inner_selector"], "0xba0cb29e")

    def test_correlates_sources_and_lead_times_by_report(self) -> None:
        decoded = decode_chainlink_calldata(
            FORWARDER,
            forward_calldata(
                AGGREGATOR,
                transmit_calldata(1_784_106_045, [187_800_000_000, 187_831_000_000, 187_900_000_000]),
            ),
        )
        state = ExperimentState()
        state.observe_candidate("public_mempool", 10_000, "pending-1", decoded)
        state.observe_candidate("mev_share", 12_000, "hint-1", decoded)

        match = state.observe_inclusion(20_000, "included-1", decoded)
        summary = state.summary()

        self.assertEqual(match["matched_sources"], ["mev_share", "public_mempool"])
        self.assertEqual(match["lead_ms_by_source"]["public_mempool"], 10_000)
        self.assertEqual(match["lead_ms_by_source"]["mev_share"], 8_000)
        self.assertEqual(summary["inclusions"], 1)
        self.assertEqual(summary["preobserved_inclusions"], 1)
        self.assertEqual(summary["seen_by_both_sources"], 1)

    def test_excludes_warmup_miss_from_coverage_denominator(self) -> None:
        decoded = decode_chainlink_calldata(
            AGGREGATOR,
            transmit_calldata(1_784_106_045, [187_800_000_000, 187_831_000_000, 187_900_000_000]),
        )
        state = ExperimentState()

        match = state.observe_inclusion(20_000, "included-before-ready", decoded, eligible=False)

        self.assertFalse(match["eligible"])
        self.assertEqual(state.summary()["inclusions"], 0)

    def test_rejects_non_oracle_transmit_payload_with_implausible_timestamp(self) -> None:
        decoded = decode_chainlink_calldata(
            "0x718672076d6d51e4c76142b37bc99e4945d704a3",
            transmit_calldata(32, [1, 2, 3, 4, 5, 32, 576]),
        )

        self.assertIsNone(decoded)


if __name__ == "__main__":
    unittest.main()

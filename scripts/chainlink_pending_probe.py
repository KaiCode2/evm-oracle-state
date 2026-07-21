#!/usr/bin/env python3
"""Passive Chainlink pending-transmission experiment.

The decoder and correlation logic are intentionally independent from the live
network clients so fixture tests can exercise the evidence we rely on.
"""

import argparse
import asyncio
import hashlib
import json
import os
import signal
import time
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional
from urllib.parse import urlsplit, urlunsplit

import aiohttp
import websockets


TRANSMIT_SELECTOR = bytes.fromhex("b1dc65a4")
TRANSMIT_SECONDARY_SELECTOR = bytes.fromhex("ba0cb29e")
FORWARD_SELECTOR = bytes.fromhex("6fadcf72")
OCR1_NEW_TRANSMISSION_TOPIC = (
    "0xf6a97944f31ea060dfde0566e4167c1a1082551e64b60ecb14d599a9d023d451"
)
OCR2_NEW_TRANSMISSION_TOPIC = (
    "0xc797025feeeaf2cd924c99e9205acb8ec04d5cad21c41ce637a38fb6dee6016a"
)
NEW_TRANSMISSION_TOPICS = {
    OCR1_NEW_TRANSMISSION_TOPIC,
    OCR2_NEW_TRANSMISSION_TOPIC,
}


def _word(data: bytes, offset: int) -> bytes:
    end = offset + 32
    if offset < 0 or end > len(data):
        raise ValueError("ABI word is out of bounds")
    return data[offset:end]


def _uint(data: bytes, offset: int) -> int:
    return int.from_bytes(_word(data, offset), "big")


def _dynamic_bytes(args: bytes, offset_position: int) -> bytes:
    offset = _uint(args, offset_position)
    length = _uint(args, offset)
    start = offset + 32
    end = start + length
    if end > len(args):
        raise ValueError("dynamic ABI bytes are out of bounds")
    return args[start:end]


def _decode_report(report: bytes) -> Dict[str, Any]:
    observations_timestamp = _uint(report, 0)
    if observations_timestamp < 1_577_836_800:
        raise ValueError("implausible Chainlink observations timestamp")
    observations_offset = _uint(report, 64)
    observation_count = _uint(report, observations_offset)
    if observation_count == 0 or observation_count > 256:
        raise ValueError("implausible Chainlink observation count")

    observations = []
    for index in range(observation_count):
        value = int.from_bytes(
            _word(report, observations_offset + 32 + index * 32),
            "big",
            signed=True,
        )
        observations.append(value)

    return {
        "observations_timestamp": observations_timestamp,
        "observation_count": observation_count,
        "median_answer": str(sorted(observations)[observation_count // 2]),
        "minimum_answer": str(min(observations)),
        "maximum_answer": str(max(observations)),
        "report_sha256": hashlib.sha256(report).hexdigest(),
    }


def decode_chainlink_calldata(to: str, calldata: str) -> Optional[Dict[str, Any]]:
    """Decode a supported Chainlink transmission, or return ``None``."""

    try:
        outer_raw = bytes.fromhex(calldata.removeprefix("0x"))
        if len(outer_raw) < 4:
            return None
        raw = outer_raw
        aggregator = to.lower()
        path = "direct_transmit"
        forwarder = None
        if outer_raw[:4] == FORWARD_SELECTOR:
            outer_args = outer_raw[4:]
            aggregator = "0x" + _word(outer_args, 0)[12:].hex()
            raw = _dynamic_bytes(outer_args, 32)
            path = "forward_transmit"
            forwarder = to.lower()
        if len(raw) < 4 or raw[:4] not in (TRANSMIT_SELECTOR, TRANSMIT_SECONDARY_SELECTOR):
            return None
        if raw[:4] == TRANSMIT_SECONDARY_SELECTOR:
            path = (
                "forward_transmit_secondary"
                if forwarder is not None
                else "direct_transmit_secondary"
            )
        report = _dynamic_bytes(raw[4:], 3 * 32)
        decoded = _decode_report(report)
    except (TypeError, ValueError):
        return None

    result = {
        "path": path,
        "aggregator": aggregator,
        "inner_selector": "0x" + raw[:4].hex(),
        "calldata_sha256": hashlib.sha256(outer_raw).hexdigest(),
        "inner_calldata_sha256": hashlib.sha256(raw).hexdigest(),
        **decoded,
    }
    if forwarder is not None:
        result["forwarder"] = forwarder
    return result


class ExperimentState:
    """Correlate speculative reports with later block inclusions."""

    def __init__(self) -> None:
        self._candidates: Dict[str, Dict[str, Dict[str, Any]]] = {}
        self._candidate_messages = defaultdict(int)
        self._inclusions = 0
        self._preobserved_inclusions = 0
        self._seen_by_both_sources = 0
        self._matched_by_source = defaultdict(int)
        self._lead_ms_by_source = defaultdict(list)

    @staticmethod
    def _key(decoded: Dict[str, Any]) -> str:
        return f"{decoded['aggregator']}:{decoded['report_sha256']}"

    def observe_candidate(
        self,
        source: str,
        seen_at_ms: int,
        source_id: str,
        decoded: Dict[str, Any],
    ) -> None:
        self._candidate_messages[source] += 1
        reports = self._candidates.setdefault(self._key(decoded), {})
        evidence = reports.setdefault(
            source,
            {
                "first_seen_ms": seen_at_ms,
                "last_seen_ms": seen_at_ms,
                "source_ids": set(),
                "calldata_hashes": set(),
            },
        )
        evidence["first_seen_ms"] = min(evidence["first_seen_ms"], seen_at_ms)
        evidence["last_seen_ms"] = max(evidence["last_seen_ms"], seen_at_ms)
        evidence["source_ids"].add(source_id)
        evidence["calldata_hashes"].add(decoded["calldata_sha256"])

    def observe_inclusion(
        self,
        included_at_ms: int,
        transaction_hash: str,
        decoded: Dict[str, Any],
        eligible: bool = True,
    ) -> Dict[str, Any]:
        candidates = self._candidates.get(self._key(decoded), {})
        leads = {
            source: included_at_ms - evidence["first_seen_ms"]
            for source, evidence in candidates.items()
            if evidence["first_seen_ms"] <= included_at_ms
        }
        sources = sorted(leads)
        result = {
            "transaction_hash": transaction_hash,
            "eligible": eligible,
            "matched_sources": sources,
            "lead_ms_by_source": leads,
            "exact_calldata_match_by_source": {
                source: decoded["calldata_sha256"] in candidates[source]["calldata_hashes"]
                for source in sources
            },
        }
        if not eligible:
            return result

        self._inclusions += 1
        if sources:
            self._preobserved_inclusions += 1
        if {"mev_share", "public_mempool"}.issubset(sources):
            self._seen_by_both_sources += 1
        for source, lead_ms in leads.items():
            self._matched_by_source[source] += 1
            self._lead_ms_by_source[source].append(lead_ms)

        return result

    def summary(self) -> Dict[str, Any]:
        lead_stats = {}
        for source, values in self._lead_ms_by_source.items():
            ordered = sorted(values)
            lead_stats[source] = {
                "count": len(ordered),
                "minimum": ordered[0],
                "p50": _percentile(ordered, 0.50),
                "p95": _percentile(ordered, 0.95),
                "maximum": ordered[-1],
            }
        coverage = {
            source: round(count * 100 / self._inclusions, 2) if self._inclusions else None
            for source, count in self._matched_by_source.items()
        }
        return {
            "candidate_messages_by_source": dict(self._candidate_messages),
            "candidate_reports_by_source": {
                source: sum(source in reports for reports in self._candidates.values())
                for source in self._candidate_messages
            },
            "inclusions": self._inclusions,
            "preobserved_inclusions": self._preobserved_inclusions,
            "missed_inclusions": self._inclusions - self._preobserved_inclusions,
            "seen_by_both_sources": self._seen_by_both_sources,
            "matched_inclusions_by_source": dict(self._matched_by_source),
            "coverage_percent_by_source": coverage,
            "lead_time_ms_summary_by_source": lead_stats,
            "lead_ms_by_source": {
                source: sorted(values) for source, values in self._lead_ms_by_source.items()
            },
        }


def _percentile(ordered: List[int], fraction: float) -> int:
    index = max(0, min(len(ordered) - 1, int((len(ordered) - 1) * fraction + 0.5)))
    return ordered[index]


def _now_ms() -> int:
    return time.time_ns() // 1_000_000


def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


class JsonlRecorder:
    def __init__(self, output_dir: Path) -> None:
        self.output_dir = output_dir
        output_dir.mkdir(parents=True, exist_ok=True)
        self.events_path = output_dir / "events.jsonl"
        self.status_path = output_dir / "status.json"
        self.summary_path = output_dir / "summary.json"
        self.pid_path = output_dir / "probe.pid"
        self._events = self.events_path.open("a", encoding="utf-8", buffering=1)
        self.pid_path.write_text(f"{os.getpid()}\n", encoding="utf-8")

    def record(self, event_type: str, **fields: Any) -> None:
        event = {"event": event_type, "recorded_at_ms": _now_ms(), **fields}
        self._events.write(json.dumps(event, separators=(",", ":"), sort_keys=True) + "\n")
        self._events.flush()

    @staticmethod
    def _write_json(path: Path, value: Dict[str, Any]) -> None:
        temporary = path.with_suffix(path.suffix + ".tmp")
        temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        temporary.replace(path)

    def write_status(self, value: Dict[str, Any]) -> None:
        self._write_json(self.status_path, value)

    def write_summary(self, value: Dict[str, Any]) -> None:
        self._write_json(self.summary_path, value)

    def close(self) -> None:
        self._events.close()


class RpcClient:
    def __init__(self, session: aiohttp.ClientSession, url: str) -> None:
        self._session = session
        self._url = url
        self._request_id = 0

    async def call(self, method: str, params: List[Any]) -> Any:
        self._request_id += 1
        payload = {
            "jsonrpc": "2.0",
            "id": self._request_id,
            "method": method,
            "params": params,
        }
        async with self._session.post(self._url, json=payload) as response:
            response.raise_for_status()
            body = await response.json()
        if body.get("error") is not None:
            raise RuntimeError(f"{method} failed: {body['error']}")
        return body.get("result")


class ProbeRunner:
    def __init__(
        self,
        http_rpc_url: str,
        ws_rpc_url: str,
        mev_share_url: str,
        recorder: JsonlRecorder,
        duration_secs: int,
        heartbeat_secs: int,
        warmup_secs: int,
    ) -> None:
        self.http_rpc_url = http_rpc_url
        self.ws_rpc_url = ws_rpc_url
        self.mev_share_url = mev_share_url
        self.recorder = recorder
        self.duration_secs = duration_secs
        self.heartbeat_secs = heartbeat_secs
        self.warmup_secs = warmup_secs
        self.state = ExperimentState()
        self.stats = defaultdict(int)
        self.stop = asyncio.Event()
        self.heads: asyncio.Queue[int] = asyncio.Queue()
        self.started_at_ms = _now_ms()
        self.stop_reason = "duration_complete"

    def _record_candidate(
        self,
        source: str,
        seen_at_ms: int,
        source_id: str,
        transaction_hash: Optional[str],
        to: str,
        calldata: str,
    ) -> None:
        decoded = decode_chainlink_calldata(to, calldata)
        if decoded is None:
            return
        self.stats[f"{source}_decoded_candidates"] += 1
        self.state.observe_candidate(source, seen_at_ms, source_id, decoded)
        self.recorder.record(
            "candidate",
            source=source,
            seen_at_ms=seen_at_ms,
            source_id=source_id,
            transaction_hash=transaction_hash,
            to=to.lower(),
            calldata=calldata,
            decoded=decoded,
        )

    async def public_mempool_and_heads(self) -> None:
        retry_secs = 1
        while not self.stop.is_set():
            try:
                async with websockets.connect(
                    self.ws_rpc_url,
                    max_size=None,
                    ping_interval=20,
                    ping_timeout=20,
                    close_timeout=5,
                ) as websocket:
                    self.stats["public_ws_connections"] += 1
                    requests = [
                        {
                            "jsonrpc": "2.0",
                            "id": 1,
                            "method": "eth_subscribe",
                            "params": ["alchemy_pendingTransactions", {"hashesOnly": False}],
                        },
                        {
                            "jsonrpc": "2.0",
                            "id": 2,
                            "method": "eth_subscribe",
                            "params": ["newHeads"],
                        },
                    ]
                    for request in requests:
                        await websocket.send(json.dumps(request))

                    subscriptions: Dict[str, str] = {}
                    while len(subscriptions) < 2:
                        message = json.loads(await asyncio.wait_for(websocket.recv(), timeout=15))
                        if message.get("error") is not None:
                            raise RuntimeError(f"subscription failed: {message['error']}")
                        if message.get("id") == 1:
                            subscriptions[message["result"]] = "pending"
                        elif message.get("id") == 2:
                            subscriptions[message["result"]] = "heads"

                    self.recorder.record("source_connected", source="public_mempool")
                    retry_secs = 1
                    async for raw_message in websocket:
                        if self.stop.is_set():
                            break
                        message = json.loads(raw_message)
                        params = message.get("params") or {}
                        kind = subscriptions.get(params.get("subscription"))
                        result = params.get("result")
                        if kind == "heads" and isinstance(result, dict) and result.get("number"):
                            self.stats["new_head_messages"] += 1
                            await self.heads.put(int(result["number"], 16))
                        elif kind == "pending":
                            self.stats["public_pending_messages"] += 1
                            if not isinstance(result, dict):
                                self.stats["public_hash_only_messages"] += 1
                                continue
                            calldata = result.get("input") or result.get("data") or ""
                            to = result.get("to")
                            if not to or not calldata:
                                continue
                            transaction_hash = result.get("hash")
                            source_id = transaction_hash or hashlib.sha256(
                                bytes.fromhex(calldata.removeprefix("0x"))
                            ).hexdigest()
                            self._record_candidate(
                                "public_mempool",
                                _now_ms(),
                                source_id,
                                transaction_hash,
                                to,
                                calldata,
                            )
            except asyncio.CancelledError:
                raise
            except Exception as error:
                self.stats["public_ws_errors"] += 1
                self.recorder.record(
                    "source_error",
                    source="public_mempool",
                    error=str(error),
                    retry_secs=retry_secs,
                )
                await self._wait_or_stop(retry_secs)
                retry_secs = min(retry_secs * 2, 30)

    async def mev_share(self, session: aiohttp.ClientSession) -> None:
        retry_secs = 1
        timeout = aiohttp.ClientTimeout(total=None, sock_connect=15, sock_read=None)
        while not self.stop.is_set():
            try:
                async with session.get(
                    self.mev_share_url,
                    headers={
                        "Accept": "text/event-stream",
                        "User-Agent": "evm-oracle-state-chainlink-probe/0.1",
                    },
                    timeout=timeout,
                ) as response:
                    response.raise_for_status()
                    self.stats["mev_share_connections"] += 1
                    self.recorder.record("source_connected", source="mev_share")
                    retry_secs = 1
                    buffer = ""
                    async for chunk in response.content.iter_chunked(65_536):
                        if self.stop.is_set():
                            break
                        buffer += chunk.decode("utf-8", "replace").replace("\r\n", "\n")
                        while "\n\n" in buffer:
                            frame, buffer = buffer.split("\n\n", 1)
                            payload = "\n".join(
                                line[5:].lstrip()
                                for line in frame.splitlines()
                                if line.startswith("data:")
                            )
                            if not payload:
                                continue
                            try:
                                event = json.loads(payload)
                            except json.JSONDecodeError:
                                self.stats["mev_share_invalid_json"] += 1
                                continue
                            self.stats["mev_share_events"] += 1
                            seen_at_ms = _now_ms()
                            hint_hash = event.get("hash") or "unknown-hint"
                            for index, transaction in enumerate(event.get("txs") or []):
                                calldata = transaction.get("callData") or ""
                                to = transaction.get("to")
                                if not to or not calldata:
                                    continue
                                self._record_candidate(
                                    "mev_share",
                                    seen_at_ms,
                                    f"{hint_hash}:{index}",
                                    None,
                                    to,
                                    calldata,
                                )
            except asyncio.CancelledError:
                raise
            except Exception as error:
                self.stats["mev_share_errors"] += 1
                self.recorder.record(
                    "source_error",
                    source="mev_share",
                    error=str(error),
                    retry_secs=retry_secs,
                )
                await self._wait_or_stop(retry_secs)
                retry_secs = min(retry_secs * 2, 30)

    async def block_inclusions(self, rpc: RpcClient, start_block: int) -> None:
        next_block = start_block + 1
        self.recorder.record("block_cursor_started", next_block=next_block)
        retry_secs = 1
        while not self.stop.is_set():
            try:
                try:
                    highest = await asyncio.wait_for(self.heads.get(), timeout=15)
                except asyncio.TimeoutError:
                    highest = int(await rpc.call("eth_blockNumber", []), 16)
                while next_block <= highest and not self.stop.is_set():
                    block = await rpc.call("eth_getBlockByNumber", [hex(next_block), True])
                    if block is None:
                        break
                    await self._observe_block(rpc, block)
                    next_block += 1
                retry_secs = 1
            except asyncio.CancelledError:
                raise
            except Exception as error:
                self.stats["block_poll_errors"] += 1
                self.recorder.record(
                    "source_error",
                    source="block_inclusions",
                    error=str(error),
                    retry_secs=retry_secs,
                    next_block=next_block,
                )
                await self._wait_or_stop(retry_secs)
                retry_secs = min(retry_secs * 2, 30)

    async def _observe_block(self, rpc: RpcClient, block: Dict[str, Any]) -> None:
        block_number = int(block["number"], 16)
        block_timestamp_ms = int(block["timestamp"], 16) * 1_000
        self.stats["blocks_observed"] += 1
        for transaction in block.get("transactions") or []:
            calldata = transaction.get("input") or transaction.get("data") or ""
            to = transaction.get("to")
            if not to or not calldata:
                continue
            decoded = decode_chainlink_calldata(to, calldata)
            if decoded is None:
                continue
            self.stats["included_chainlink_attempts"] += 1
            transaction_hash = transaction["hash"]
            receipt = await rpc.call("eth_getTransactionReceipt", [transaction_hash])
            success = receipt is not None and int(receipt.get("status", "0x0"), 16) == 1
            transmission_logs = []
            if success:
                transmission_logs = [
                    log
                    for log in receipt.get("logs") or []
                    if log.get("address", "").lower() == decoded["aggregator"]
                    and (log.get("topics") or [""])[0].lower() in NEW_TRANSMISSION_TOPICS
                ]
            base = {
                "block_number": block_number,
                "block_hash": block.get("hash"),
                "block_timestamp_ms": block_timestamp_ms,
                "transaction_hash": transaction_hash,
                "transaction_index": int(transaction.get("transactionIndex", "0x0"), 16),
                "success": success,
                "new_transmission_log_count": len(transmission_logs),
                "to": to.lower(),
                "calldata": calldata,
                "decoded": decoded,
            }
            if not transmission_logs:
                self.stats["included_unconfirmed_chainlink_attempts"] += 1
                self.recorder.record("included_attempt", **base)
                continue

            self.stats["confirmed_chainlink_updates"] += 1
            preview = self.state.observe_inclusion(
                block_timestamp_ms,
                transaction_hash,
                decoded,
                eligible=False,
            )
            eligible = (
                block_timestamp_ms >= self.started_at_ms + self.warmup_secs * 1_000
                or bool(preview["matched_sources"])
            )
            if eligible:
                match = self.state.observe_inclusion(
                    block_timestamp_ms,
                    transaction_hash,
                    decoded,
                )
            else:
                self.stats["warmup_excluded_updates"] += 1
                match = preview
            self.recorder.record("inclusion", **base, correlation=match)

    async def heartbeat(self) -> None:
        while not self.stop.is_set():
            await self._wait_or_stop(self.heartbeat_secs)
            if self.stop.is_set():
                break
            status = self.status("running")
            self.recorder.write_status(status)
            self.recorder.record(
                "heartbeat",
                elapsed_secs=status["elapsed_secs"],
                transport_stats=status["transport_stats"],
                experiment_summary=status["experiment_summary"],
            )

    def status(self, run_status: str) -> Dict[str, Any]:
        return {
            "status": run_status,
            "pid": os.getpid(),
            "started_at": datetime.fromtimestamp(
                self.started_at_ms / 1_000, timezone.utc
            ).isoformat(),
            "updated_at": _utc_now(),
            "elapsed_secs": round((_now_ms() - self.started_at_ms) / 1_000, 3),
            "configured_duration_secs": self.duration_secs,
            "configured_warmup_secs": self.warmup_secs,
            "stop_reason": self.stop_reason if run_status != "running" else None,
            "transport_stats": dict(self.stats),
            "experiment_summary": self.state.summary(),
        }

    async def _wait_or_stop(self, seconds: float) -> None:
        try:
            await asyncio.wait_for(self.stop.wait(), timeout=seconds)
        except asyncio.TimeoutError:
            pass

    async def run(self) -> Dict[str, Any]:
        timeout = aiohttp.ClientTimeout(total=30)
        connector = aiohttp.TCPConnector(limit=20)
        async with aiohttp.ClientSession(timeout=timeout, connector=connector) as session:
            rpc = RpcClient(session, self.http_rpc_url)
            start_block = int(await rpc.call("eth_blockNumber", []), 16)
            self.recorder.record(
                "session_started",
                started_at=_utc_now(),
                duration_secs=self.duration_secs,
                warmup_secs=self.warmup_secs,
                start_block=start_block,
                http_rpc=_redact_url(self.http_rpc_url),
                ws_rpc=_redact_url(self.ws_rpc_url),
                mev_share_url=self.mev_share_url,
            )
            self.recorder.write_status(self.status("running"))

            tasks = [
                asyncio.create_task(self.public_mempool_and_heads(), name="public-mempool"),
                asyncio.create_task(self.mev_share(session), name="mev-share"),
                asyncio.create_task(
                    self.block_inclusions(rpc, start_block), name="block-inclusions"
                ),
                asyncio.create_task(self.heartbeat(), name="heartbeat"),
            ]
            timer = asyncio.create_task(asyncio.sleep(self.duration_secs), name="duration")
            stop_waiter = asyncio.create_task(self.stop.wait(), name="stop-waiter")
            done, _ = await asyncio.wait(
                [*tasks, timer, stop_waiter],
                return_when=asyncio.FIRST_COMPLETED,
            )

            failed = [task for task in done if task in tasks]
            if failed and not self.stop.is_set():
                task = failed[0]
                self.stop_reason = f"task_failed:{task.get_name()}:{task.exception()}"
            elif stop_waiter in done and not timer.done():
                self.stop_reason = "signal"
            else:
                self.stop_reason = "duration_complete"
            self.stop.set()
            for task in [*tasks, timer, stop_waiter]:
                if not task.done():
                    task.cancel()
            await asyncio.gather(*tasks, timer, stop_waiter, return_exceptions=True)

        final = self.status("complete" if self.stop_reason == "duration_complete" else "stopped")
        self.recorder.record("session_finished", **final)
        self.recorder.write_status(final)
        self.recorder.write_summary(final)
        return final


def _load_env_file(path: Path) -> None:
    if not path.exists():
        return
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        key = key.removeprefix("export ").strip()
        value = value.strip().strip('"').strip("'")
        os.environ.setdefault(key, value)


def _rpc_urls() -> tuple:
    http_rpc = os.environ.get("ETH_RPC_URL") or os.environ.get("E2E_RPC_URL")
    if not http_rpc:
        raise RuntimeError("ETH_RPC_URL or E2E_RPC_URL is required")
    ws_rpc = os.environ.get("ETH_WS_RPC_URL") or os.environ.get("WS_RPC_URL")
    if not ws_rpc:
        parsed = urlsplit(http_rpc)
        scheme = "wss" if parsed.scheme == "https" else "ws"
        ws_rpc = urlunsplit((scheme, parsed.netloc, parsed.path, parsed.query, parsed.fragment))
    return http_rpc, ws_rpc


def _redact_url(value: str) -> str:
    parsed = urlsplit(value)
    host = parsed.hostname or "unknown"
    if parsed.port is not None:
        host = f"{host}:{parsed.port}"
    return f"{parsed.scheme}://{host}/..."


def _default_output_dir() -> Path:
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return Path("target") / "chainlink-pending-probe" / timestamp


def _arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--duration-secs", type=int, default=60)
    parser.add_argument("--heartbeat-secs", type=int, default=60)
    parser.add_argument("--warmup-secs", type=int, default=30)
    parser.add_argument("--output-dir", type=Path, default=None)
    parser.add_argument("--env-file", type=Path, default=Path(".env"))
    parser.add_argument("--mev-share-url", default="https://mev-share.flashbots.net")
    return parser.parse_args()


async def _main() -> int:
    arguments = _arguments()
    if arguments.duration_secs <= 0 or arguments.heartbeat_secs <= 0 or arguments.warmup_secs < 0:
        raise ValueError("duration and heartbeat must be positive; warmup cannot be negative")
    _load_env_file(arguments.env_file)
    http_rpc, ws_rpc = _rpc_urls()
    output_dir = arguments.output_dir or _default_output_dir()
    recorder = JsonlRecorder(output_dir)
    runner = ProbeRunner(
        http_rpc,
        ws_rpc,
        arguments.mev_share_url,
        recorder,
        arguments.duration_secs,
        arguments.heartbeat_secs,
        arguments.warmup_secs,
    )
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, runner.stop.set)
    try:
        final = await runner.run()
        print(json.dumps({"output_dir": str(output_dir.resolve()), **final}, indent=2))
        return 0 if final["status"] == "complete" else 1
    finally:
        recorder.close()


if __name__ == "__main__":
    raise SystemExit(asyncio.run(_main()))

"""Tests for the JSON router config file support."""

import json
import pathlib

import pytest
from vllm_router.config_file import ConfigFileError, load_config_file
from vllm_router.launch_router import parse_router_args


def _write_config(tmp_path, config):
    path = tmp_path / "router.json"
    path.write_text(json.dumps(config), encoding="utf-8")
    return str(path)


class TestRouterConfigFile:
    def test_all_options_can_come_from_config(self, tmp_path):
        config_path = _write_config(
            tmp_path,
            {
                "host": "0.0.0.0",
                "port": 30001,
                "worker_urls": ["http://worker1:8000", "http://worker2:8000"],
                "policy": "consistent_hash",
                "worker_startup_timeout_secs": 120,
                "retry_max_retries": 3,
                "request_id_headers": ["x-session-id"],
                "service_discovery": True,
                "selector": {"app": "worker", "env": "prod"},
                "vllm_discovery_address": "0.0.0.0:30001",
            },
        )

        args = parse_router_args(["--config", config_path])

        assert args.host == "0.0.0.0"
        assert args.port == 30001
        assert args.worker_urls == ["http://worker1:8000", "http://worker2:8000"]
        assert args.policy == "consistent_hash"
        assert args.worker_startup_timeout_secs == 120
        assert args.retry_max_retries == 3
        assert args.request_id_headers == ["x-session-id"]
        assert args.service_discovery is True
        assert args.selector == {"app": "worker", "env": "prod"}
        assert args.vllm_discovery_address == "0.0.0.0:30001"

    def test_pd_prefill_decode_from_config(self, tmp_path):
        config_path = _write_config(
            tmp_path,
            {
                "vllm_pd_disaggregation": True,
                "prefill": [["http://prefill1:8000", "9000"], ["http://prefill2:8000"]],
                "decode": ["http://decode1:8001", "http://decode2:8001"],
                "prefill_policy": "consistent_hash",
                "decode_policy": "consistent_hash",
            },
        )

        args = parse_router_args(["--config", config_path])

        assert args.vllm_pd_disaggregation is True
        assert args.prefill_urls == [
            ("http://prefill1:8000", 9000),
            ("http://prefill2:8000", None),
        ]
        assert args.decode_urls == ["http://decode1:8001", "http://decode2:8001"]
        assert args.prefill_policy == "consistent_hash"
        assert args.decode_policy == "consistent_hash"

    def test_cli_arguments_override_config_scalars(self, tmp_path):
        config_path = _write_config(
            tmp_path,
            {
                "host": "1.2.3.4",
                "port": 30001,
                "worker_urls": ["http://worker1:8000"],
                "policy": "consistent_hash",
            },
        )

        args = parse_router_args(
            ["--config", config_path, "--host", "9.9.9.9", "--port", "31111"]
        )

        assert args.host == "9.9.9.9"
        assert args.port == 31111
        # Config list values are used when no CLI value is supplied.
        assert args.worker_urls == ["http://worker1:8000"]

    def test_cli_arguments_replace_config_lists(self, tmp_path):
        config_path = _write_config(
            tmp_path,
            {
                "worker_urls": ["http://config1:8000", "http://config2:8000"],
                "policy": "random",
            },
        )

        args = parse_router_args(
            ["--config", config_path, "--worker-urls", "http://cli:8000"]
        )

        assert args.worker_urls == ["http://cli:8000"]

    def test_unknown_config_key_raises(self, tmp_path):
        config_path = _write_config(tmp_path, {"not_an_option": 1})

        with pytest.raises(ConfigFileError, match="unknown config option"):
            parse_router_args(["--config", config_path])

    def test_missing_config_file_raises(self, tmp_path):
        with pytest.raises(ConfigFileError, match="cannot read"):
            parse_router_args(["--config", str(tmp_path / "missing.json")])

    def test_load_config_file_returns_dict(self, tmp_path):
        config_path = _write_config(tmp_path, {"policy": "round_robin"})
        assert load_config_file(config_path) == {"policy": "round_robin"}

    def test_parse_without_config_preserves_cli(self, tmp_path):
        args = parse_router_args(
            ["--host", "1.2.3.4", "--policy", "random"]
        )
        assert args.host == "1.2.3.4"
        assert args.policy == "random"

    def test_example_full_config_parses(self):
        example = (
            pathlib.Path(__file__).resolve().parents[2]
            / "examples"
            / "configs"
            / "router_config.example.json"
        )
        args = parse_router_args(["--config", str(example)])

        assert args.host == "127.0.0.1"
        assert args.port == 30000
        assert args.worker_urls == [
            "http://worker1:8000",
            "http://worker2:8000",
        ]
        assert args.policy == "consistent_hash"
        assert args.retry_max_retries == 5
        assert args.health_check_endpoint == "/health"
        assert args.cb_failure_threshold == 10

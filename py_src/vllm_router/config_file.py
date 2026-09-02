"""Load a JSON router config file and turn it into CLI-style arguments.

The official ``vllm-router`` launcher historically receives every control
option through argparse parameters. To make "everything can also be configured
through a single config file", we load the config file and translate it back
into CLI tokens *before* the real CLI arguments. When the same option is also
supplied on the command line, the command line value wins for that option.

Config keys are the argparse destination names (snake_case, e.g.
``worker_urls``, ``vllm_pd_disaggregation``), matching the field names on
:class:`vllm_router.router_args.RouterArgs`. Unknown keys raise
:class:`ConfigFileError` with the list of accepted keys.
"""

from __future__ import annotations

import argparse
import json
import pathlib
from typing import Any, Dict, List


class ConfigFileError(ValueError):
    """Raised when a router config file cannot be loaded or mapped."""


# Config files may use the RouterArgs dataclass names in addition to the
# argparse destinations (which differ for a few options).
CONFIG_ALIASES = {
    "decode_urls": "decode",
    "prefill_urls": "prefill",
    "eviction_interval": "eviction_interval_secs",
}


def _find_config_path(argv: List[str]) -> str | None:
    for i, arg in enumerate(argv):
        if arg == "--config" and i + 1 < len(argv):
            return argv[i + 1]
        if arg.startswith("--config="):
            return arg.split("=", 1)[1]
    return None


def load_config_file(path: str | pathlib.Path) -> Dict[str, Any]:
    """Read and parse a router JSON config file."""
    config_path = pathlib.Path(path)
    try:
        raw = config_path.read_text(encoding="utf-8")
    except OSError as exc:
        raise ConfigFileError(f"cannot read config file '{config_path}': {exc}") from exc
    try:
        config = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise ConfigFileError(
            f"invalid JSON in config file '{config_path}': {exc}"
        ) from exc
    if not isinstance(config, dict):
        raise ConfigFileError(
            f"config file '{config_path}' must contain a JSON object"
        )
    return config


def _selector_dict_to_list(key: str, value: Any) -> Any:
    """Allow selectors to be written either as dicts or "k=v" lists."""
    if isinstance(value, dict) and key.endswith("selector"):
        return [f"{k}={v}" for k, v in value.items()]
    return value


def _append_entries(action: argparse.Action, value: Any) -> List[str]:
    """Convert a config value for an argparse 'append' action to CLI tokens."""
    if not isinstance(value, list):
        raise ConfigFileError(
            f"option '{action.dest}' expects a list, got {type(value).__name__}"
        )
    option = action.option_strings[0]
    tokens: List[str] = []
    if action.nargs == "+":  # --prefill URL [PORT] ...
        for entry in value:
            if isinstance(entry, (list, tuple)):
                parts = [str(part) for part in entry]
            else:
                parts = [str(entry)]
            if not parts:
                continue
            tokens.append(option)
            tokens.extend(parts)
    else:  # nargs == 1, e.g. --decode URL
        for entry in value:
            tokens.append(option)
            tokens.append(str(entry))
    return tokens


def _store_tokens(action: argparse.Action, value: Any) -> List[str]:
    option = action.option_strings[0]
    if isinstance(action, argparse._StoreTrueAction):  # type: ignore[attr-defined]
        return [option] if value is True else []
    if value is None:
        return []

    nargs = action.nargs
    if nargs is None:
        return [option, str(value)]
    if nargs in ("*", "+") or isinstance(nargs, int):
        if not isinstance(value, list):
            raise ConfigFileError(
                f"option '{action.dest}' expects a list, got {type(value).__name__}"
            )
        if not value:
            return []
        tokens = [option]
        for item in value:
            tokens.append(str(item))
        return tokens
    raise ConfigFileError(f"unsupported nargs for option '{action.dest}'")


def _raw_has_option(argv: List[str], option: str) -> bool:
    return any(arg == option or arg.startswith(f"{option}=") for arg in argv)


def config_to_cli_args(
    parser: argparse.ArgumentParser,
    config: Dict[str, Any],
    argv: List[str],
) -> List[str]:
    """Translate a config object into CLI tokens for an argparse parser."""
    by_dest = {
        action.dest: action
        for action in parser._actions
        if action.option_strings and action.dest not in ("help", "config")
    }
    allowed = sorted(by_dest)
    tokens: List[str] = []

    for key, raw_value in config.items():
        if key == "config":
            raise ConfigFileError(
                "'config' cannot be nested inside a config file"
            )
        action = by_dest.get(CONFIG_ALIASES.get(key, key))
        if action is None:
            raise ConfigFileError(
                f"unknown config option '{key}'. Supported options: {', '.join(allowed)}"
            )
        if _raw_has_option(argv, action.option_strings[0]):
            continue
        value = _selector_dict_to_list(key, raw_value)
        if isinstance(action, argparse._AppendAction):  # type: ignore[attr-defined]
            tokens.extend(_append_entries(action, value))
        else:
            tokens.extend(_store_tokens(action, value))
    return tokens


def parse_with_config(
    parser: argparse.ArgumentParser, argv: List[str]
) -> argparse.Namespace:
    """Parse argv, loading ``--config <file>`` first when present."""
    config_path = _find_config_path(argv)
    if config_path is None:
        return parser.parse_args(argv)
    config = load_config_file(config_path)
    config_tokens = config_to_cli_args(parser, config, argv)
    # Config tokens come first; real CLI tokens come after so that explicit
    # command line scalars override the file.
    return parser.parse_args(config_tokens + list(argv))

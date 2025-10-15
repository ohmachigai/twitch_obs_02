#!/usr/bin/env python3
"""Generate short-lived overlay/admin JWTs for local testing.

Defaults match the development configuration (`dev-sse-secret-change-me`).
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import hmac
import json
import time
from typing import Any, Dict

DEFAULT_KEY_HEX = "6465762d7373652d7365637265742d6368616e67652d6d65"


def base64url(data: bytes) -> bytes:
    return base64.urlsafe_b64encode(data).rstrip(b"=")


def encode_segment(value: Dict[str, Any]) -> bytes:
    return base64url(json.dumps(value, separators=(",", ":"), sort_keys=True).encode())


def generate_token(subject: str, audience: str, key_hex: str, ttl: int, *, not_before: int | None) -> str:
    key = bytes.fromhex(key_hex)
    now = int(time.time())
    header = {"alg": "HS256", "typ": "JWT"}
    payload: Dict[str, Any] = {"sub": subject, "aud": audience, "iat": now, "exp": now + ttl}
    if not_before is not None:
        payload["nbf"] = now + not_before

    header_segment = encode_segment(header)
    payload_segment = encode_segment(payload)
    signing_input = header_segment + b"." + payload_segment
    signature = base64url(hmac.new(key, signing_input, hashlib.sha256).digest())
    return (signing_input + b"." + signature).decode()


def main() -> None:
    parser = argparse.ArgumentParser(description="Generate overlay/admin JWTs for local testing")
    parser.add_argument("subject", help="Broadcaster ID for the token (JWT 'sub')")
    parser.add_argument("audience", choices=["overlay", "admin"], help="JWT audience ('aud')")
    parser.add_argument(
        "--key-hex",
        default=DEFAULT_KEY_HEX,
        help="HMAC-SHA256 signing key as hex (defaults to dev-sse-secret-change-me)",
    )
    parser.add_argument("--ttl", type=int, default=600, help="Token lifetime in seconds (default: 600)")
    parser.add_argument(
        "--nbf-offset",
        type=int,
        default=None,
        help="Optional not-before offset in seconds (added to current epoch)",
    )
    args = parser.parse_args()

    token = generate_token(
        subject=args.subject,
        audience=args.audience,
        key_hex=args.key_hex,
        ttl=args.ttl,
        not_before=args.nbf_offset,
    )
    print(token)


if __name__ == "__main__":
    main()

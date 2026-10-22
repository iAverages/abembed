#! /usr/bin/env nix-shell
#! nix-shell -i python3 -p python3Packages.cryptography
"""Copy accounts between environments, decrypting with the source ACCOUNT_KEY.

    scripts/copy-account.py                       # prompts for source, target and key
    scripts/copy-account.py --from staging --to prod --platform instagram

The source key comes from SOURCE_ACCOUNT_KEY, or .dev.vars for local, else a prompt. The target
admin token works like add-account.py.
"""
import argparse
import base64
import getpass
import importlib.util
import json
import os
import subprocess
import sys
from pathlib import Path

from cryptography.exceptions import InvalidTag
from cryptography.hazmat.primitives.ciphers.aead import AESGCM

spec = importlib.util.spec_from_file_location("add_account", Path(__file__).parent / "add-account.py")
add_account = importlib.util.module_from_spec(spec)
spec.loader.exec_module(add_account)

WRANGLER_ARGS = {
    "local": ["--local"],
    "staging": ["--remote", "--env", "staging"],
    "prod": ["--remote", "--env", "production"],
}


def accounts(env, platform):
    where = f"WHERE platform = '{platform}'" if platform else ""
    output = subprocess.run(
        ["pnpm", "dlx", "wrangler@4", "d1", "execute", "ACCOUNTS", "--json", *WRANGLER_ARGS[env],
         "--command", f"SELECT platform, account_id, label, region, credential, nonce FROM accounts {where}"],
        cwd=Path(__file__).parent.parent,
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    return json.loads(output)[0]["results"]


def decrypt(key, row):
    aad = f"{row['platform']}:{row['account_id']}".encode()
    plaintext = AESGCM(base64.b64decode(key)).decrypt(
        base64.b64decode(row["nonce"]), base64.b64decode(row["credential"]), aad
    )
    return json.loads(plaintext)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--from", dest="source", choices=WRANGLER_ARGS)
    parser.add_argument("--to", dest="target", choices=add_account.ENVS)
    parser.add_argument("--platform", choices=["facebook", "instagram"])
    opts = parser.parse_args()

    source = opts.source or add_account.ask("Copy from", list(WRANGLER_ARGS), "staging")
    target = opts.target or add_account.ask("Copy to", list(add_account.ENVS), "prod")
    rows = accounts(source, opts.platform)
    if not rows:
        sys.exit(f"No accounts on {source}")

    key = os.environ.get("SOURCE_ACCOUNT_KEY")
    if not key and source == "local":
        key = add_account.dev_var("ACCOUNT_KEY")
    key = key or getpass.getpass(f"ACCOUNT_KEY for {source}: ")

    for row in rows:
        try:
            credential = decrypt(key, row)
        except InvalidTag:
            sys.exit(f"ACCOUNT_KEY does not decrypt {row['platform']} {row['account_id']} on {source}")
        print(f"{row['platform']} {row['account_id']} ({row['label']}) -> {target}")
        add_account.upload(
            target,
            {
                "platform": row["platform"],
                "region": row["region"],
                "label": row["label"],
                "cookie": "; ".join(f"{name}={value}" for name, value in credential["cookies"].items()),
                "headers": credential["headers"],
            },
        )


if __name__ == "__main__":
    main()

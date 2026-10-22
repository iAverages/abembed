#!/usr/bin/env python3
"""Upload a logged in browser session as an abembed account.

Copy a request to the platform from the browser devtools as cURL, then:

    scripts/add-account.py            # prompts for everything, paste the curl, finish with ctrl-d
    scripts/add-account.py --env local --file request.curl

The admin token comes from ADMIN_TOKEN, or .dev.vars for local.
"""
import argparse
import getpass
import json
import os
import shlex
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

ENVS = {
    "local": "http://127.0.0.1:8787",
    "staging": "https://staging.abembed.com",
    "prod": "https://abembed.com",
}
PLATFORMS = {"facebook.com": "facebook", "instagram.com": "instagram"}
REGIONS = "abcd"


def parse_curl(text):
    args = shlex.split(text.replace("\\\n", " "))
    url, headers, cookie = None, {}, None
    i = 1
    while i < len(args):
        arg = args[i]
        if arg in ("-H", "--header"):
            name, _, value = args[i + 1].partition(":")
            headers[name.strip().lower()] = value.strip()
            i += 2
        elif arg in ("-b", "--cookie"):
            cookie = args[i + 1]
            i += 2
        elif arg == "--url":
            url = args[i + 1]
            i += 2
        elif arg.startswith("-"):
            i += 2 if arg in ("-X", "--request", "--data-raw", "--data", "-d") else 1
        else:
            url = arg
            i += 1
    cookie = cookie or headers.pop("cookie", None)
    if not url or not cookie:
        sys.exit("The curl command needs a url and a cookie header")
    return url, headers, cookie


def ask(prompt, choices, default):
    while True:
        answer = input(f"{prompt} [{'/'.join(choices)}] ({default}): ").strip() or default
        if answer in choices:
            return answer


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--env", choices=ENVS)
    parser.add_argument("--region", choices=list(REGIONS))
    parser.add_argument("--label")
    parser.add_argument("--file", help="file with the curl command, defaults to stdin")
    opts = parser.parse_args()

    env = opts.env or ask("Upload to", list(ENVS), "local")
    region = opts.region or ask("Provider region", list(REGIONS), "a")
    label = opts.label if opts.label is not None else input("Label (optional): ").strip()

    if opts.file:
        text = Path(opts.file).read_text()
    else:
        print("Paste the curl command, then press ctrl-d:")
        text = sys.stdin.read()
    url, headers, cookie = parse_curl(text)
    host = urllib.parse.urlsplit(url).hostname or ""
    platform = next((p for domain, p in PLATFORMS.items() if host.endswith(domain)), None)
    if not platform:
        sys.exit(f"Unsupported platform host {host}")

    upload(
        env,
        {
            "platform": platform,
            "region": region,
            "label": label or None,
            "cookie": cookie,
            "headers": headers,
        },
    )


def admin_token(env):
    token = os.environ.get("ADMIN_TOKEN")
    if not token and env == "local":
        token = dev_var("ADMIN_TOKEN")
    return token or getpass.getpass(f"Admin token for {env}: ")


def dev_var(name):
    for line in (Path(__file__).parent.parent / ".dev.vars").read_text().splitlines():
        if line.startswith(f"{name}="):
            return line.split("=", 1)[1].strip().strip('"')
    return None


def upload(env, body):
    request = urllib.request.Request(
        f"{ENVS[env]}/admin/accounts",
        data=json.dumps(body).encode(),
        headers={
            "Authorization": f"Bearer {admin_token(env)}",
            "Content-Type": "application/json",
            # cloudflare's browser integrity check blocks the python-urllib user agent
            "User-Agent": "abembed-scripts",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            print(response.read().decode())
    except urllib.error.HTTPError as error:
        sys.exit(f"{error.code}: {error.read().decode()}")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Build the access request (Word attachment + email text) from docs/access-request/ and local/access-request.env.

The templates in docs/access-request/ are organisation-neutral; the organisation's values live in the gitignored
local/access-request.env (start from docs/access-request/request.example.env). Output goes to dist/.
"""
import datetime
import math
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SRC = ROOT / "docs" / "access-request"
ENV = ROOT / "local" / "access-request.env"
DIST = ROOT / "dist"

# SharePoint per-app limits per tenant, by licensed users (docs/sharepoint/throttling.md).
TIERS = [(1_000, 1), (5_000, 2), (15_000, 3), (50_000, 4), (math.inf, 5)]
DAY_UNIT, MINUTE_UNIT = 1_200_000, 1_250

# Measured costs (docs/sharepoint/throttling.md, "Measured cost").
IDLE_LIVE, IDLE_POLL = 190, 1_870             # RU per library per day
NOTIFY_TYPICAL, NOTIFY_BUSY = 100, 2_880      # notification batches per library per day
DOWN, UP = 200 * 1.5, 50 * 4.8                # typical changes per person per day: 200 down, 50 up
FIRST_SYNC_RU_PER_MIN = 1_000                 # upper end of one person's first sync at the client's pacing


def load_env(path):
    if not path.exists():
        sys.exit(f"missing {path.relative_to(ROOT)}: copy docs/access-request/request.example.env there and fill it in")
    values = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            k, v = line.split("=", 1)
            values[k.strip()] = v.strip()
    return values


def n(x):
    return f"{round(x):,}"


def pct(x):
    return f"{x * 100:.1f}%"


def attributes(env):
    licenses, users, libs = int(env["LICENSES"]), int(env["PILOT_USERS"]), int(env["LIBRARIES_PER_USER"])
    mult = next(m for top, m in TIERS if licenses <= top)
    day, minute = DAY_UNIT * mult, MINUTE_UNIT * mult
    work = DOWN + UP
    typical = libs * (IDLE_LIVE + NOTIFY_TYPICAL) + work
    polling = libs * IDLE_POLL + work
    busy = libs * (IDLE_LIVE + NOTIFY_BUSY) + work
    return {
        "org": env["ORG"],
        "app-name": env["APP_NAME"],
        "requester": env["REQUESTER"],
        "requester-role": env.get("REQUESTER_ROLE", ""),
        "date": datetime.date.today().isoformat(),
        "licenses": f"{licenses:,}",
        "pilot-users": f"{users:,}",
        "libs": str(libs),
        "ru-day": n(day),
        "ru-min": n(minute),
        "typ-total": n(users * typical),
        "typ-pct": pct(users * typical / day),
        "poll-total": n(users * polling),
        "poll-pct": pct(users * polling / day),
        "busy-total": n(users * busy),
        "busy-pct": pct(users * busy / day),
        "typ-min-pct": pct(users * typical / 1440 / minute),
        "capacity": f"{round(day / typical, -2):,.0f}",
        "first-sync-max": str(max(1, minute // FIRST_SYNC_RU_PER_MIN)),
    }


def main():
    attrs = attributes(load_env(ENV))
    DIST.mkdir(exist_ok=True)
    slug = re.sub(r"[^A-Za-z0-9]+", "-", attrs["org"]).strip("-")
    docx = DIST / f"{slug}-access-request.docx"
    email = DIST / f"{slug}-access-request-email.txt"

    args = ["asciidoctor", "-b", "docbook5", "-o", "-"]
    for k, v in attrs.items():
        args += ["-a", f"{k}={v}"]
    docbook = subprocess.run(args + [str(SRC / "attachment.adoc")], check=True, capture_output=True).stdout
    subprocess.run(["pandoc", "-f", "docbook", "-t", "docx", "--toc", "-o", str(docx)], input=docbook, check=True)

    body = (SRC / "email.md").read_text()
    body = re.sub(r"\{([a-z-]+)\}", lambda m: attrs.get(m.group(1), m.group(0)), body)
    if left := re.findall(r"\{[a-z-]+\}", body):
        sys.exit(f"email.md has unknown placeholders: {left}")
    email.write_text(body)

    print(f"attachment: {docx.relative_to(ROOT)}")
    print(f"email:      {email.relative_to(ROOT)}")


if __name__ == "__main__":
    main()

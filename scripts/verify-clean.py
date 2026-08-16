#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["boto3>=1.40"]
# ///
# SPDX-License-Identifier: Apache-2.0
"""Ask the account what this project left behind, rather than trusting teardown.

Teardown reporting success and the account being clean are different questions,
and the difference has cost us twice. `terraform destroy` once reported nine
resources destroyed while six CloudWatch log groups survived, because the service
creates those itself and Terraform never owned them. Separately an image deletion
retried past the point where the log-group delete had already run, leaving the
group behind again.

So this queries the account directly. It is what the live suite's own report
cannot be: independent of the code that did the cleanup.

Three outcomes rather than two, because collapsing them trains people to ignore
the script. A *leak* is something still costing money that nothing intends to
keep: a live MicroVM, an image, a log group. *Standing* is the Terraform stack,
which a caller may keep applied on purpose. *Pending* is a deletion still in
flight, where the right response is to re-run in a minute.

Exit 0 when nothing leaked, 1 otherwise. `--delete` removes the leaks — the
honest response to one you have just proven exists — and leaves the Terraform
stack to `terraform destroy`, which owns it.
"""

from __future__ import annotations

import argparse
import os
import sys
from typing import Any

import boto3

REGION = os.environ.get("AWS_REGION", "us-east-1")
# Everything this project creates carries one of these markers. Anything else in
# the account is somebody else's and must not be touched.
# Every prefix anything in this repo can create. Keeping this list complete is the
# whole correctness condition: a missing prefix makes the checker report "clean"
# while a resource bills, which is worse than having no checker at all — it
# converts an unknown into a false assurance. The `microvm-cli` entry was added
# after exactly that happened: a CLI run leaked a log group and this script said
# the account was clean, because it only knew the two names the conformance
# scripts used.
NAME_PREFIXES = ("agentd-conformance", "agentd-probe", "microvm-cli", "microvm-")
LOG_GROUP_PREFIXES = (
    "/aws/lambda-microvms/agentd-",
    "/aws/lambda-microvms/microvm-",
)


def ours(name: str | None) -> bool:
    return bool(name) and name.startswith(NAME_PREFIXES)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--delete",
        action="store_true",
        help="remove what is found instead of only reporting it",
    )
    args = parser.parse_args()

    session = boto3.Session(region_name=REGION)
    mv = session.client("lambda-microvms")
    s3 = session.client("s3")
    iam = session.client("iam")
    logs = session.client("logs")

    leaks: list[str] = []

    # MicroVMs. TERMINATED is not a leak: billing stops at terminate and the
    # record is history the service keeps. Anything else is still costing money.
    live_states = {"PENDING", "RUNNING", "SUSPENDING", "SUSPENDED", "TERMINATING"}
    for vm in mv.list_microvms().get("items", []):
        if vm.get("state") in live_states:
            leaks.append(f"microvm {vm.get('microvmId')} in {vm.get('state')}")

    # Images. DELETING is in progress rather than leaked, so it is reported
    # separately: re-running this a minute later is the right response.
    pending: list[str] = []
    for image in mv.list_microvm_images().get("items", []):
        name = image.get("name")
        if not ours(name):
            continue
        state = image.get("state")
        if state == "DELETING":
            pending.append(f"image {name} still DELETING")
        else:
            leaks.append(f"image {name} in {state}")

    # The bucket and roles belong to the Terraform stack, which a caller may keep
    # applied deliberately between runs — re-applying costs a couple of minutes and
    # the idle cost is pennies. So they are reported as standing infrastructure
    # rather than as leaks: calling a deliberate choice a leak trains people to
    # ignore this script, and an ignored leak check is the same as none.
    standing: list[str] = []
    for bucket in s3.list_buckets().get("Buckets", []):
        if ours(bucket.get("Name")):
            standing.append(f"bucket {bucket['Name']}")

    paginator = iam.get_paginator("list_roles")
    for page in paginator.paginate():
        for role in page.get("Roles", []):
            if ours(role.get("RoleName")):
                standing.append(f"iam role {role['RoleName']}")

    # The one Terraform cannot see, and therefore the one most likely to survive.
    log_groups: list[dict[str, Any]] = []
    for prefix in LOG_GROUP_PREFIXES:
        log_groups.extend(
            logs.describe_log_groups(logGroupNamePrefix=prefix).get("logGroups", [])
        )
    for group in log_groups:
        leaks.append(f"log group {group['logGroupName']}")

    for note in pending:
        print(f"  pending: {note}")
    for note in standing:
        print(f"  standing (terraform-owned): {note}")

    if not leaks:
        print(f"account {REGION}: clean — no conformance resources survive")
        return 0

    print(f"account {REGION}: {len(leaks)} leaked resource(s)")
    for leak in leaks:
        print(f"  LEAK {leak}")

    if not args.delete:
        print("\nre-run with --delete to remove them")
        return 1

    print("\ndeleting")
    for group in log_groups:
        _try(logs.delete_log_group, logGroupName=group["logGroupName"])
    for image in mv.list_microvm_images().get("items", []):
        if ours(image.get("name")) and image.get("state") != "DELETING":
            _try(
                mv.delete_microvm_image,
                imageIdentifier=image.get("imageIdentifier") or image.get("imageArn"),
            )
    for vm in mv.list_microvms().get("items", []):
        if vm.get("state") in live_states:
            _try(mv.terminate_microvm, microvmIdentifier=vm["microvmId"])
    # Buckets and roles are Terraform-owned, so `terraform destroy` is the right
    # tool and deleting them here would desync the state file.
    print("buckets and IAM roles are Terraform-owned: run")
    print("  terraform -chdir=conformance/infra destroy")
    return 1


def _try(fn: Any, **kwargs: Any) -> None:
    label = ", ".join(f"{k}={v}" for k, v in kwargs.items())
    try:
        fn(**kwargs)
        print(f"  deleted {label}")
    except Exception as exc:  # noqa: BLE001 - report and continue; one failure is not fatal
        print(f"  could not delete {label}: {type(exc).__name__}")


if __name__ == "__main__":
    sys.exit(main())

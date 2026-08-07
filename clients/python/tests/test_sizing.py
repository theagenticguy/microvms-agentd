"""The sizing table: what a baseline request buys, and what it bills.

`minimumMemoryInMiB` selects a class rather than sizing a VM, and the class's two
numbers differ by 4x. Measured 2026-08-07: requesting 512 produced a guest
reporting ~2 GB `MemTotal`, requesting 2048 produced ~8 GB. So the number a caller
writes is neither the memory they get nor, on its own, the rate they pay — which is
why the table is code with tests rather than a paragraph in a docstring.
"""

from __future__ import annotations

import pytest

from microvms_agentd.sizing import (
    DEFAULT_BASELINE_MIB,
    SIZE_CLASSES,
    default_size_class,
    size_class_for,
)


def test_the_two_measured_rows_report_the_peak_the_guest_reported() -> None:
    # The only two rows anyone has actually observed. Asserted against the measured
    # `MemTotal` figures rather than against 4x the baseline: computing the peak from
    # the baseline would make this test agree with the implementation by construction
    # instead of with AWS, and a future row that breaks the 4x pattern would be
    # silently wrong in both places at once.
    assert size_class_for(512).peak_mib == 2048, "512 MiB requested reported ~2 GB MemTotal"
    assert size_class_for(2048).peak_mib == 8192, "2048 MiB requested reported ~8 GB MemTotal"


def test_billing_reads_the_baseline_and_never_the_peak() -> None:
    # "You pay the baseline rate while your MicroVM is running and only pay for what
    # you actively use above the baseline." The peak is burst headroom, so a cost
    # calculation that reaches for the guest-reported figure over-states the bill 4x.
    size = size_class_for(2048)
    assert size.baseline_gb == 2.0
    assert size.baseline_vcpu == 1
    assert size.peak_gb == 8.0


def test_an_off_table_baseline_is_rejected_rather_than_snapped() -> None:
    # 1500 has two plausible readings — round up to the enclosing class, or take the
    # literal as a baseline — that differ in both the memory the guest gets and the
    # rate it is billed at. Neither is documented and neither was measured, so a
    # client that guessed would hand someone a wrong bill they cannot notice.
    with pytest.raises(ValueError, match="not a documented size class baseline"):
        size_class_for(1500)


def test_the_rejection_names_the_classes_a_caller_can_pick() -> None:
    # An error that only says "invalid" sends the reader to the AWS console. The five
    # baselines are the entire domain, so naming them is the whole fix.
    with pytest.raises(ValueError) as caught:
        size_class_for(3072)
    for baseline in (512, 1024, 2048, 4096, 8192):
        assert str(baseline) in str(caught.value)


def test_the_default_is_the_platforms_own_default_not_the_cheapest_class() -> None:
    # 0.5 GB bills less per second, but baseline is also the *floor* of the burst
    # range and the guest has no swap (`SwapTotal: 0 kB`), so pressure goes straight
    # to the OOM killer with no paging phase to absorb a low default. Cheap-and-broken
    # is a worse default than adequate; ~3 cents an hour is not worth a sandbox that
    # kills a real test suite.
    assert DEFAULT_BASELINE_MIB == 2048
    assert default_size_class().baseline_gb == 2.0
    assert SIZE_CLASSES[0].baseline_mib == 512, "the cheapest class exists but is not the default"


def test_describe_names_both_numbers_so_neither_can_be_read_alone() -> None:
    # Both or neither. Naming only the peak invites budgeting for memory that is never
    # billed; naming only the baseline invites a memory-pressure test written against
    # a ceiling four times below the one the guest actually enforces.
    text = size_class_for(1024).describe()
    assert "1 GB" in text and "0.5 vCPU" in text
    assert "4 GB" in text and "2 vCPU" in text
    assert "billed while running" in text

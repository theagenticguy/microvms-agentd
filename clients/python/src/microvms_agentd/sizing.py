"""Size classes: what `minimumMemoryInMiB` actually buys, and what it bills.

`minimumMemoryInMiB` does not size a VM. It selects a *class*, and the class has
two numbers that differ by 4x — the baseline you are billed for every running
second, and the peak the guest reports in `/proc/meminfo`. Measured 2026-08-07,
us-east-1: requesting 512 produced a guest reporting ~2 GB `MemTotal`; requesting
2048 produced ~8 GB. Both match AWS's documented sizing table exactly, so the
number the caller writes down is neither the number they get nor, on its own,
enough to predict the bill.

That is why the table is a module rather than a comment. A caller who wants to
know what a request will produce asks `size_class_for`; a caller computing cost
reads `baseline_gb` and `baseline_vcpu`, never the peak.

Only the five table values are accepted. AWS documents what each of them means and
we measured two of them; what the service does with an off-table request such as
1500 is undocumented and unmeasured, and the two plausible behaviors — round up to
the enclosing class, or take the literal as a baseline — differ in both the memory
the guest gets and the rate it is billed at. Rejecting locally costs the caller a
second; guessing costs them a wrong bill they have no way to notice.
"""

from __future__ import annotations

from dataclasses import dataclass

#: MiB per GB as AWS's pricing and sizing tables use it. The guest's `MemTotal`
#: reads in KiB and lands slightly under (2 GB shows as 2037648 kB), so any
#: comparison against a guest-reported figure is approximate by nature.
MIB_PER_GB = 1024


@dataclass(frozen=True)
class SizeClass:
    """One row of the documented sizing table: a baseline and its burst ceiling.

    Frozen because these are measurements of someone else's service, not settings.
    A caller who mutates one is describing a platform that does not exist.
    """

    #: What `minimumMemoryInMiB` must be set to in order to select this class.
    baseline_mib: int
    #: Billed per running second, alongside `baseline_vcpu`. Not what the guest reports.
    baseline_vcpu: float
    #: What the guest reports as `MemTotal`, and the ceiling a burst can reach.
    #: Charged only for the seconds above baseline that are actually consumed.
    peak_mib: int
    peak_vcpu: float

    @property
    def baseline_gb(self) -> float:
        """The figure a GB-second rate multiplies. Always the baseline, never the peak."""
        return self.baseline_mib / MIB_PER_GB

    @property
    def peak_gb(self) -> float:
        return self.peak_mib / MIB_PER_GB

    def describe(self) -> str:
        """One line naming both numbers, for an error message or a CLI.

        Both numbers or neither: naming only the peak invites someone to budget for
        memory they will not be billed for, and naming only the baseline invites a
        memory-pressure test written against a ceiling four times lower than the one
        the guest enforces.
        """
        return (
            f"{self.baseline_gb:g} GB / {self.baseline_vcpu:g} vCPU baseline "
            f"(billed while running), bursting to {self.peak_gb:g} GB / "
            f"{self.peak_vcpu:g} vCPU (what the guest reports)"
        )


#: The documented table (`microvms-images.html`), smallest first. Every peak is 4x
#: its baseline; that regularity is AWS's, and a guard below holds us to reading it
#: from the table rather than computing it, so a future row that breaks the pattern
#: does not silently get the pattern applied to it.
SIZE_CLASSES: tuple[SizeClass, ...] = (
    SizeClass(baseline_mib=512, baseline_vcpu=0.25, peak_mib=2048, peak_vcpu=1),
    SizeClass(baseline_mib=1024, baseline_vcpu=0.5, peak_mib=4096, peak_vcpu=2),
    SizeClass(baseline_mib=2048, baseline_vcpu=1, peak_mib=8192, peak_vcpu=4),
    SizeClass(baseline_mib=4096, baseline_vcpu=2, peak_mib=16384, peak_vcpu=8),
    SizeClass(baseline_mib=8192, baseline_vcpu=4, peak_mib=32768, peak_vcpu=16),
)

#: The platform's own default, and ours. Deliberately not the smallest class: the
#: baseline is also the floor of the burst range, and a 0.5 GB default hands someone
#: a sandbox that OOM-kills a real test suite to save about three cents an hour.
#: Guest swap is absent (`SwapTotal: 0 kB`), so there is no paging phase to absorb
#: the mistake — pressure goes straight to the OOM killer.
DEFAULT_BASELINE_MIB = 2048


def size_class_for(baseline_mib: int) -> SizeClass:
    """The class `minimumMemoryInMiB=baseline_mib` selects.

    Rejects anything not in the table rather than snapping to a neighbor. See the
    module docstring: an off-table request has two plausible readings that differ in
    both memory and rate, and we have measured neither.
    """
    for size in SIZE_CLASSES:
        if size.baseline_mib == baseline_mib:
            return size
    offered = ", ".join(str(s.baseline_mib) for s in SIZE_CLASSES)
    raise ValueError(
        f"minimumMemoryInMiB={baseline_mib} is not a documented size class baseline. "
        f"The field selects a class, it does not size a VM; pass one of {offered} MiB. "
        "See docs/PLATFORM.md, '`minimumMemoryInMiB` selects a *baseline*, and the "
        "guest reports the *peak*'."
    )


def default_size_class() -> SizeClass:
    return size_class_for(DEFAULT_BASELINE_MIB)

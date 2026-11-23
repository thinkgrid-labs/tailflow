"""Per-process resource counters for benchmark harnesses.

`snapshot(pid)` returns cumulative counters for one process, read without
root and without sampling tools:

  cpu_s           user + system CPU seconds
  rss_bytes       resident set size
  private_bytes   memory the process alone is charged for
                  (macOS: phys_footprint; Linux: Private_Clean + Private_Dirty)
  wakeups         macOS: package-idle + interrupt wakeups (what Activity
                  Monitor's "Idle Wake Ups" counts). Linux: voluntary +
                  involuntary context switches summed over threads, the
                  closest unprivileged equivalent.
  disk_write_bytes  bytes the process caused to be written to storage
  disk_read_bytes

Counters are cumulative; diff two snapshots for a window. Missing values are
None, never zero, so a platform gap is not mistaken for "idle".
"""

import ctypes
import ctypes.util
import os
import sys

_RUSAGE_INFO_V2 = 2


class _RusageInfoV2(ctypes.Structure):
    _fields_ = [
        ("ri_uuid", ctypes.c_uint8 * 16),
        ("ri_user_time", ctypes.c_uint64),
        ("ri_system_time", ctypes.c_uint64),
        ("ri_pkg_idle_wkups", ctypes.c_uint64),
        ("ri_interrupt_wkups", ctypes.c_uint64),
        ("ri_pageins", ctypes.c_uint64),
        ("ri_wired_size", ctypes.c_uint64),
        ("ri_resident_size", ctypes.c_uint64),
        ("ri_phys_footprint", ctypes.c_uint64),
        ("ri_proc_start_abstime", ctypes.c_uint64),
        ("ri_proc_exit_abstime", ctypes.c_uint64),
        ("ri_child_user_time", ctypes.c_uint64),
        ("ri_child_system_time", ctypes.c_uint64),
        ("ri_child_pkg_idle_wkups", ctypes.c_uint64),
        ("ri_child_interrupt_wkups", ctypes.c_uint64),
        ("ri_child_pageins", ctypes.c_uint64),
        ("ri_child_elapsed_abstime", ctypes.c_uint64),
        ("ri_diskio_bytesread", ctypes.c_uint64),
        ("ri_diskio_byteswritten", ctypes.c_uint64),
    ]


class _MachTimebase(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


_libc = None
_ns_per_tick = 1.0
if sys.platform == "darwin":
    _libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
    _libc.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
    _libc.proc_pid_rusage.restype = ctypes.c_int
    # rusage_info times are in mach absolute-time ticks (1 ns on Intel,
    # ~41.67 ns on Apple silicon).
    tb = _MachTimebase()
    _libc.mach_timebase_info(ctypes.byref(tb))
    if tb.denom:
        _ns_per_tick = tb.numer / tb.denom


def _darwin(pid):
    info = _RusageInfoV2()
    if _libc.proc_pid_rusage(pid, _RUSAGE_INFO_V2, ctypes.byref(info)) != 0:
        return None
    return {
        "cpu_s": (info.ri_user_time + info.ri_system_time) * _ns_per_tick / 1e9,
        "rss_bytes": info.ri_resident_size,
        "private_bytes": info.ri_phys_footprint,
        "wakeups": info.ri_pkg_idle_wkups + info.ri_interrupt_wkups,
        "disk_write_bytes": info.ri_diskio_byteswritten,
        "disk_read_bytes": info.ri_diskio_bytesread,
    }


def _read(path):
    try:
        with open(path) as f:
            return f.read()
    except OSError:
        return None


def _linux(pid):
    stat = _read(f"/proc/{pid}/stat")
    if stat is None:
        return None
    # Fields after the parenthesised comm, which may itself contain spaces.
    fields = stat.rsplit(")", 1)[1].split()
    tick = os.sysconf("SC_CLK_TCK")
    cpu_s = (int(fields[11]) + int(fields[12])) / tick
    rss_bytes = int(fields[21]) * os.sysconf("SC_PAGE_SIZE")
    private = None
    rollup = _read(f"/proc/{pid}/smaps_rollup")
    if rollup:
        kb = 0
        for line in rollup.splitlines():
            if line.startswith(("Private_Clean:", "Private_Dirty:")):
                kb += int(line.split()[1])
        private = kb * 1024
    switches = 0
    try:
        for tid in os.listdir(f"/proc/{pid}/task"):
            st = _read(f"/proc/{pid}/task/{tid}/status") or ""
            for line in st.splitlines():
                if line.startswith(("voluntary_ctxt_switches:", "nonvoluntary_ctxt_switches:")):
                    switches += int(line.split()[1])
    except OSError:
        switches = None
    io = {}
    for line in (_read(f"/proc/{pid}/io") or "").splitlines():
        k, _, v = line.partition(":")
        io[k.strip()] = int(v)
    return {
        "cpu_s": cpu_s,
        "rss_bytes": rss_bytes,
        "private_bytes": private,
        "wakeups": switches,
        "disk_write_bytes": io.get("write_bytes"),
        "disk_read_bytes": io.get("read_bytes"),
    }


def snapshot(pid):
    if sys.platform == "darwin":
        return _darwin(pid)
    if sys.platform.startswith("linux"):
        return _linux(pid)
    return None


def delta(a, b):
    """Counter differences b - a (gauges taken from b)."""
    if a is None or b is None:
        return None
    out = {}
    for k in ("cpu_s", "wakeups", "disk_write_bytes", "disk_read_bytes"):
        out[k] = None if a.get(k) is None or b.get(k) is None else b[k] - a[k]
    out["rss_bytes"] = b.get("rss_bytes")
    out["private_bytes"] = b.get("private_bytes")
    return out


if __name__ == "__main__":
    import json

    print(json.dumps(snapshot(int(sys.argv[1])), indent=2))

"""GDB-only disposable host-side trace asset; never calls an inferior function."""

from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import struct

import gdb

CONFIG = json.loads(Path(os.environ["WARM_ALLOCATOR_CONFIG"]).read_text())
TRACE = {"diagnostic_only": True, "acceptance_comparator_run": False,
         "workload": CONFIG["workload"], "scope": CONFIG["boundary"]["scope"],
         "callgrind_unscoped_calls_used": False,
         "event_count_interpretation": "observed allocator API entries only while one wrapper is active; not Ir and not global calls= counters",
         "target_allocator_mutation": False, "target_function_calls": False,
         "entries": 0, "returns": 0, "events": [], "errors": [], "scope_valid": False,
         "api_breakpoints": [], "unavailable_apis": [], "inferior_exit_code": None}
ACTIVE = False
THREAD = None
ENTRY_SP = None
SRET = None
PENDING = {}
BREAKPOINTS = []


def integer(register):
    return int(gdb.parse_and_eval(f"${register}")) & ((1 << 64) - 1)


def words(address, count):
    data = bytes(gdb.selected_inferior().read_memory(address, count * 8))
    return list(struct.unpack("<" + "Q" * count, data))


def pointer_metadata(pointer):
    if not pointer:
        return {"pointer": 0, "status": "null"}
    # Only read inferior bytes. Without validated libc build/layout provenance these
    # words are deliberately NOT converted to usable/chunk/copy sizes.
    try:
        value = words(pointer - 16, 2)
        return {"pointer": pointer, "raw_preceding_u64": value,
                "chunk_bytes": None, "usable_bytes": None,
                "interpretation": "unknown: raw bytes only; no libc layout/flag assumptions"}
    except gdb.error as error:
        return {"pointer": pointer, "status": "unreadable", "error": str(error)}


def allocator_state():
    result = {"method": "read-only GDB libc DWARF fields, no inferior calls",
              "tcache": {"status": "unknown"}, "main_arena": {"status": "unknown"}}
    try:
        value = gdb.parse_and_eval("tcache")
        target = value.type.strip_typedefs().target().strip_typedefs()
        fields = {field.name: field for field in target.fields()}
        if not {"counts", "entries"} <= fields.keys():
            raise ValueError("tcache DWARF lacks counts/entries")
        lo, hi = fields["counts"].type.strip_typedefs().range()
        elo, ehi = fields["entries"].type.strip_typedefs().range()
        if lo != 0 or elo != 0 or hi != ehi or not 0 < hi < 128:
            raise ValueError("Unsupported tcache array bounds")
        if fields["entries"].type.strip_typedefs().target().sizeof != 8:
            raise ValueError("Unsupported tcache entry pointer width")
        provenance = {"type": str(target), "bytes": target.sizeof,
                      "counts_bitpos": fields["counts"].bitpos, "entries_bitpos": fields["entries"].bitpos,
                      "array_entries": hi + 1}
        if int(value) == 0:
            result["tcache"] = {"status": "known_null", "dwarf_layout": provenance}
        else:
            cache = value.dereference()
            result["tcache"] = {"status": "read", "pointer": int(value), "dwarf_layout": provenance,
                                "counts": [int(cache["counts"][index]) for index in range(hi + 1)],
                                "entry_pointers": [int(cache["entries"][index]) for index in range(hi + 1)],
                                "size_class_bytes": "unknown: layout fields do not prove tunable bin-to-size mapping"}
    except (gdb.error, ValueError, TypeError) as error:
        result["tcache"]["reason"] = str(error)
    try:
        arena = gdb.parse_and_eval("main_arena")
        target = arena.type.strip_typedefs()
        fields = {field.name: field for field in target.fields()}
        if not {"bins", "top", "last_remainder"} <= fields.keys():
            raise ValueError("malloc_state DWARF lacks required fields")
        lo, hi = fields["bins"].type.strip_typedefs().range()
        if lo != 0 or not 0 < hi < 256 or fields["bins"].type.strip_typedefs().target().sizeof != 8:
            raise ValueError("Unsupported arena bin pointer array")
        result["main_arena"] = {"status": "read", "dwarf_layout": {"type": str(target), "bytes": target.sizeof,
                                  "bins_bitpos": fields["bins"].bitpos, "top_bitpos": fields["top"].bitpos},
                                  "top_pointer": int(arena["top"]), "last_remainder_pointer": int(arena["last_remainder"]),
                                  "raw_bin_link_pointers": [int(arena["bins"][index]) for index in range(hi + 1)],
                                  "bin_classification": "unknown: raw link slots only"}
    except (gdb.error, ValueError, TypeError) as error:
        result["main_arena"]["reason"] = str(error)
    return result


def libc_provenance():
    maps = Path(f"/proc/{gdb.selected_inferior().pid}/maps").read_text()
    files = {}
    for row in maps.splitlines():
        fields = row.split(maxsplit=5)
        if len(fields) == 6 and fields[5].startswith("/") and "libc.so" in Path(fields[5]).name:
            path = Path(fields[5])
            if str(path) not in files:
                try:
                    files[str(path)] = {"sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                except OSError as error:
                    files[str(path)] = {"status": "unknown", "reason": str(error)}
    return {"mapped_libc_files": files,
            "dwarf_objfiles": [objfile.filename for objfile in gdb.objfiles() if "libc" in objfile.filename],
            "process_maps": maps}


def failed(error):
    TRACE["errors"].append(str(error))
    return True


def check_thread():
    if gdb.selected_thread().global_num != THREAD:
        raise ValueError("Allocator callback on another thread during the scoped query")


class AllocationReturn(gdb.FinishBreakpoint):
    def __init__(self, event):
        super().__init__(gdb.newest_frame(), internal=True)
        self.event = event
        PENDING[event["id"]] = self

    def stop(self):
        try:
            check_thread()
            if not ACTIVE:
                raise ValueError("Allocation returned outside wrapper scope")
            event = self.event
            returned = integer("rax")
            event["return_register_rax"] = returned
            if event["api"] == "posix_memalign":
                event["status"] = returned & 0xffffffff
                pointer = words(event["out_pointer"], 1)[0] if event["status"] == 0 else 0
            else:
                pointer = returned
            event["returned_pointer"] = pointer
            event["returned_metadata"] = pointer_metadata(pointer)
            event["completed"] = True
            if event["api"] == "realloc":
                event["relocated"] = pointer != event["old_pointer"] if pointer else None
                event["failed_nonzero_request"] = pointer == 0 and event["requested_bytes"] != 0
            else:
                event["failed_nonzero_request"] = pointer == 0 and event.get("requested_bytes", 0) != 0
            PENDING.pop(event["id"], None)
            return False
        except (gdb.error, ValueError, OSError) as error:
            return failed(error)

    def out_of_scope(self):
        failed(f"Allocation return breakpoint left scope: event {self.event['id']}")
        PENDING.pop(self.event["id"], None)


class AllocationEntry(gdb.Breakpoint):
    def __init__(self, api, address):
        super().__init__(f"*0x{address:x}", internal=True)
        self.api = api
        self.address = address

    def stop(self):
        if not ACTIVE:
            return False
        try:
            check_thread()
            if len(TRACE["events"]) >= CONFIG["max_events"]:
                raise ValueError("Trace event buffer bound exceeded; no truncated success")
            a, b, c = integer("rdi"), integer("rsi"), integer("rdx")
            sp = integer("rsp")
            event = {"id": len(TRACE["events"]), "api": self.api, "callee_pc": integer("pc"),
                     "caller_return_pc": words(sp, 1)[0], "thread": THREAD,
                     "active_wrapper_entry": TRACE["entries"], "alignment": None,
                     "nested_allocator_api": bool(PENDING), "completed": self.api == "free"}
            if self.api in {"malloc", "valloc", "pvalloc"}:
                event["requested_bytes"] = a
            elif self.api == "calloc":
                event.update({"count": a, "element_bytes": b, "requested_bytes": a * b,
                              "size_product_overflows_u64": a * b >= 1 << 64})
            elif self.api == "realloc":
                event.update({"old_pointer": a, "old_metadata": pointer_metadata(a), "requested_bytes": b})
            elif self.api in {"aligned_alloc", "memalign"}:
                event.update({"alignment": a, "requested_bytes": b})
            elif self.api == "posix_memalign":
                event.update({"out_pointer": a, "alignment": b, "requested_bytes": c})
            elif self.api == "free":
                event.update({"old_pointer": a, "old_metadata": pointer_metadata(a)})
            TRACE["events"].append(event)
            if self.api != "free":
                AllocationReturn(event)
            return False
        except (gdb.error, ValueError, OSError) as error:
            return failed(error)


def install_allocator_breakpoints():
    addresses = {}
    for api in ("malloc", "realloc", "free", "calloc", "aligned_alloc", "memalign", "posix_memalign", "valloc", "pvalloc"):
        try:
            address = int(gdb.parse_and_eval(f"(unsigned long)&{api}"))
            if address in addresses:
                TRACE["unavailable_apis"].append({"api": api, "reason": f"same entry address as {addresses[address]}"})
                continue
            addresses[address] = api
            BREAKPOINTS.append(AllocationEntry(api, address))
            TRACE["api_breakpoints"].append({"api": api, "address": address})
        except gdb.error as error:
            TRACE["unavailable_apis"].append({"api": api, "reason": str(error)})
    required = {entry["api"] for entry in TRACE["api_breakpoints"]}
    if not {"malloc", "realloc", "free"} <= required:
        raise ValueError("Cannot resolve required libc API entry breakpoints")


class QueryEntry(gdb.Breakpoint):
    def stop(self):
        global ACTIVE, THREAD, ENTRY_SP, SRET
        try:
            if ACTIVE or TRACE["entries"]:
                raise ValueError("Query wrapper must enter exactly once")
            TRACE["entries"] += 1
            THREAD = gdb.selected_thread().global_num
            ENTRY_SP = integer("rsp")
            SRET = integer("rdi")
            TRACE["entry"] = {"pc": integer("pc"), "rsp": ENTRY_SP, "caller_return_pc": words(ENTRY_SP, 1)[0],
                              "rust_sret_address": SRET, "allocator_state": allocator_state(),
                              "libc_provenance": libc_provenance()}
            install_allocator_breakpoints()
            ACTIVE = True
            return False
        except (gdb.error, ValueError, OSError) as error:
            return failed(error)


class QueryReturn(gdb.Breakpoint):
    def stop(self):
        global ACTIVE
        try:
            check_thread()
            if not ACTIVE or TRACE["returns"] or integer("rsp") != ENTRY_SP:
                raise ValueError("Own wrapper ret does not match one entry/stack frame")
            if PENDING:
                raise ValueError("Uncompleted allocation requests at wrapper return")
            TRACE["returns"] += 1
            ACTIVE = False
            TRACE["return"] = {"pc": integer("pc"), "rsp": integer("rsp"),
                               "rust_sret_words": words(SRET, 3),
                               "sret_interpretation": "raw Rust return-storage words; Vec field order not assumed",
                               "allocator_state": allocator_state()}
            return False
        except (gdb.error, ValueError, OSError) as error:
            return failed(error)


def exited(event):
    TRACE["inferior_exit_code"] = getattr(event, "exit_code", None)
    if TRACE["inferior_exit_code"] is None:
        TRACE["errors"].append("Inferior exit code unavailable (signal or abnormal termination)")


def mapped_base():
    inferior = gdb.selected_inferior()
    executable = os.path.realpath(CONFIG["executable"])
    maps = Path(f"/proc/{inferior.pid}/maps").read_text()
    TRACE["entry_process_maps"] = maps
    matches = []
    for row in maps.splitlines():
        fields = row.split(maxsplit=5)
        if len(fields) == 6 and int(fields[2], 16) == 0 and os.path.realpath(fields[5]) == executable:
            matches.append(int(fields[0].split("-", 1)[0], 16))
    if len(matches) != 1:
        raise ValueError(f"Cannot uniquely resolve offset-zero executable mapping: {matches}")
    base = matches[0] - CONFIG["elf_layout"]["offset_zero_vaddr"]
    if CONFIG["elf_layout"]["kind"] == "EXEC" and base != 0:
        raise ValueError("Unexpected relocation of an EXEC ELF")
    return base


def run():
    try:
        gdb.execute("set pagination off")
        gdb.execute("set confirm off")
        gdb.execute("set debuginfod enabled off")
        gdb.execute("set auto-load off")
        gdb.execute("set disable-randomization on")
        gdb.execute("set language c")
        gdb.execute("unset environment WARM_ALLOCATOR_CONFIG")
        gdb.events.exited.connect(exited)
        # Stop at loader start, before any fixture allocation. Resolving the main
        # executable mapping does not force constructors or inferior calls.
        gdb.execute("starti")
        base = mapped_base()
        boundary = CONFIG["boundary"]
        TRACE["load_bias"] = base
        TRACE["boundary"] = boundary
        BREAKPOINTS.append(QueryEntry(f"*0x{base + boundary['address']:x}", internal=True))
        for address in boundary["return_addresses"]:
            BREAKPOINTS.append(QueryReturn(f"*0x{base + address:x}", internal=True))
        gdb.execute("continue")
        if ACTIVE or PENDING or TRACE["entries"] != 1 or TRACE["returns"] != 1:
            raise ValueError("Missing or ambiguous wrapper entry/return")
        if TRACE["inferior_exit_code"] != 0:
            raise ValueError(f"Inferior did not exit successfully: {TRACE['inferior_exit_code']}")
    except (gdb.error, ValueError, OSError) as error:
        failed(error)
    finally:
        TRACE["request_counts"] = dict(Counter(event["api"] for event in TRACE["events"] if event["api"] != "free"))
        TRACE["free_count"] = sum(event["api"] == "free" for event in TRACE["events"])
        TRACE["observed_scoped_event_count"] = len(TRACE["events"])
        TRACE["completed_request_count"] = sum(event["api"] != "free" and event["completed"] for event in TRACE["events"])
        TRACE["scope_valid"] = not TRACE["errors"] and TRACE["entries"] == 1 and TRACE["returns"] == 1 and TRACE["inferior_exit_code"] == 0
        Path(CONFIG["output"]).write_text(json.dumps(TRACE, indent=2, allow_nan=False) + "\n")
    gdb.execute(f"quit {0 if TRACE['scope_valid'] else 1}")


run()

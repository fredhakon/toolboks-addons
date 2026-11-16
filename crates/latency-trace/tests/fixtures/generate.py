# Writes the synthetic tracerpt-XML fixtures for crates/latency-trace/tests/fixtures.
import datetime as dt, sys, os
OUT = sys.argv[1]
EPOCH = dt.datetime(1601, 1, 1)
BASE_UTC = dt.datetime(2026, 11, 16, 9, 0, 0)   # trace start, UTC
SKEW = dt.timedelta(hours=1)                    # local wall clock = UTC + 1 h
def ft(t):  # datetime (UTC) -> FILETIME
    d = t - EPOCH
    return (d.days * 86400 + d.seconds) * 10_000_000 + d.microseconds * 10
def at(sec):  # seconds after trace start -> UTC datetime
    return BASE_UTC + dt.timedelta(microseconds=round(sec * 1_000_000))
def wall(t):
    w = t + SKEW
    return w.strftime("%Y-%m-%dT%H:%M:%S.") + f"{w.microsecond:06d}" + "00+00:59"

PERF = "{ce1dbfb4-137e-4da6-87b0-3f59aa102cbc}"
IMG = "{2cb15d1d-5fc1-11d2-abe1-00a0c911f518}"
HDR = "{68fdd900-4a3e-11d1-84f4-0000f80464e3}"
KERNEL = "{9e814aad-3204-11d2-9a82-006008a86939}"

def event(opcode, opname, evname, guid, time_text, cpu, data, pid=0, tid=0):
    rows = "\n".join(f'\t\t<Data Name="{k}">{v}</Data>' for k, v in data)
    return f'''<Event xmlns="http://schemas.microsoft.com/win/2004/08/events/event">
	<System>
		<Provider Guid="{KERNEL}" />
		<EventID>0</EventID>
		<Version>2</Version>
		<Level>0</Level>
		<Task>0</Task>
		<Opcode>{opcode}</Opcode>
		<Keywords>0x0</Keywords>
		<TimeCreated SystemTime="{time_text}" />
		<Correlation ActivityID="{{00000000-0000-0000-0000-000000000000}}" />
		<Execution ProcessID="{pid}" ThreadID="{tid}" ProcessorID="{cpu}" KernelTime="0" UserTime="0" />
		<Channel />
		<Computer />
	</System>
	<EventData>
{rows}
	</EventData>
	<RenderingInfo Culture="en-US">
		<Opcode>{opname}</Opcode>
		<Provider>MSNT_SystemTrace</Provider>
		<EventName xmlns="http://schemas.microsoft.com/win/2004/08/events/trace">{evname}</EventName>
	</RenderingInfo>
	<ExtendedTracingInfo xmlns="http://schemas.microsoft.com/win/2004/08/events/trace">
		<EventGuid>{guid}</EventGuid>
	</ExtendedTracingInfo>
</Event>
'''

def header(length_s, events_lost=0, buffers_lost=0):
    start = BASE_UTC
    return event(0, "Header", "EventTrace", HDR, wall(start), 0, [
        ("BufferSize", "    1024"), ("Version", "83951626"), ("ProviderVersion", "   26300"),
        ("NumberOfProcessors", "       8"), ("EndTime", ft(at(length_s))),
        ("TimerResolution", "  156250"), ("MaxFileSize", "       0"), ("LogFileMode", "0x00000001"),
        ("BuffersWritten", "      40"), ("StartBuffers", "       1"), ("PointerSize", "       8"),
        ("EventsLost", f"{events_lost:8d}"), ("CPUSpeed", "    3792"), ("LoggerName", "0x9"),
        ("LogFileName", "0x6"), ("BootTime", ft(BASE_UTC - dt.timedelta(days=2))),
        ("PerfFreq", "10000000"), ("StartTime", ft(start)), ("ReservedFlags", "0x1"),
        ("BuffersLost", f"{buffers_lost:8d}"), ("SessionNameString", "NT Kernel Logger"),
    ])

def image(path, base, size, pid=0, opcode=3, opname="DCStart", sec=0.0):
    return event(opcode, opname, "Image", IMG, wall(at(sec)), 0, [
        ("ImageBase", f"0x{base:X}"), ("ImageSize", f"0x{size:X}"), ("ProcessId", f"{pid:8d}"),
        ("ImageCheckSum", "  123456"), ("TimeDateStamp", "1700000000"), ("SignatureLevel", "0"),
        ("SignatureType", "0"), ("Reserved0", "0"), ("DefaultBase", f"0x{base:X}"),
        ("Reserved1", "0"), ("Reserved2", "0"), ("Reserved3", "0"), ("Reserved4", "0"),
        ("FileName", path),
    ], pid=4)

KINDS = {"ThreadDPC": 66, "ISR": 67, "DPC": 68, "TimerDPC": 69}
def run(kind, cpu, start_s, us, routine, end_text=None, initial=None):
    """A DPC/ISR on `cpu` entered at start_s seconds that ran `us` microseconds (inclusive)."""
    start = at(start_s)
    end = start + dt.timedelta(microseconds=us)
    data = [("InitialTime", initial if initial is not None else ft(start)), ("Routine", f"0x{routine:X}")]
    if kind == "ISR":
        data += [("ReturnValue", "1"), ("Vector", "  130"), ("Reserved", "0x0")]
    return event(KINDS[kind], kind, "PerfInfo", PERF, end_text or wall(end), cpu, data)

NTOS = 0xFFFFF80000000000; NDIS = 0xFFFFF80010000000; NV = 0xFFFFF80020000000
STOR = 0xFFFFF80030000000; MOUSE = 0xFFFFF80040000000; AUDIO = 0xFFFFF80050000000
def kernel_images(extra=()):
    out = [
        image(r"\SystemRoot\system32\ntoskrnl.exe", NTOS, 0x1000000),
        image(r"\SystemRoot\System32\drivers\ndis.sys", NDIS, 0x200000),
        image(r"\SystemRoot\System32\DriverStore\FileRepository\nv_dispi.inf_amd64_0123\nvlddmkm.sys", NV, 0x4000000),
        image(r"\SystemRoot\System32\drivers\storport.sys", STOR, 0x100000),
    ]
    return out + list(extra)

def write(name, parts):
    with open(os.path.join(OUT, name), "w", encoding="utf-8", newline="\r\n") as f:
        f.write("<Events>\n" + "".join(parts) + "</Events>\n")

# basic.xml: one run per driver kind, clear ranking, a user-mode image and a driver from a user folder.
write("basic.xml", [header(2.0)] + kernel_images([
    image(r"C:\Users\alice\Downloads\coolmouse\coolmouse.sys", MOUSE, 0x10000),
    image(r"C:\Users\alice\AppData\Local\Temp\helper.dll", 0x7FF600000000, 0x20000, pid=4321, opcode=10, opname="Load", sec=0.05),
]) + [
    run("DPC", 0, 0.100000, 20, NDIS + 0x1234),
    run("DPC", 0, 0.500000, 35, NDIS + 0x1234),
    run("DPC", 1, 0.700000, 250, NV + 0x8000),
    run("ISR", 1, 0.900000, 8, NV + 0x9000),
    run("ISR", 2, 1.200000, 30, STOR + 0x400),
    run("TimerDPC", 3, 1.500000, 5, MOUSE + 0x10),
    run("DPC", 4, 1.600000, 12, 0x7FF600001000),
])

# nested.xml: runs interrupted by runs on the same CPU, and a run on another CPU at the same time.
write("nested.xml", [header(3.0)] + kernel_images() + [
    # CPU 0: ndis DPC 1.000000-1.000100 interrupted by a storport ISR 1.000040-1.000070.
    run("ISR", 0, 1.000040, 30, STOR + 0x400),
    run("ISR", 2, 1.000050, 10, STOR + 0x400),       # other CPU, same moment: not nested
    run("DPC", 0, 1.000000, 100, NDIS + 0x1234),
    # storport alone on CPU 3: 80 us, longer than ndis' own 70 us but shorter than its elapsed 100 us.
    run("ISR", 3, 1.500000, 80, STOR + 0x400),
    # CPU 1, three levels: threaded DPC 2.000000-2.000500 > DPC 2.000100-2.000200 > ISR 2.000120-2.000150.
    run("ISR", 1, 2.000120, 30, STOR + 0x400),
    run("DPC", 1, 2.000100, 100, NDIS + 0x1234),
    run("ThreadDPC", 1, 2.000000, 500, NV + 0x8000),
])

# unresolved.xml: a routine in no image, and a null routine.
write("unresolved.xml", [header(1.0), image(r"\SystemRoot\System32\drivers\ndis.sys", NDIS, 0x200000)] + [
    run("DPC", 0, 0.100000, 20, NDIS + 0x10),
    run("DPC", 0, 0.200000, 20, NDIS + 0x10),
    run("DPC", 0, 0.300000, 20, NDIS + 0x10),
    run("DPC", 1, 0.400000, 50, 0xFFFFF800DEAD0000),
    run("ISR", 1, 0.500000, 10, 0x0),
])

# spike.xml: no header (clock offset learned from the first run); a short spike on CPU 5.
# The drivers are listed only by the end-of-trace rundown (DCEnd); audiodrv's ImageBase is
# written in lower-case hex.
write("spike.xml", [
    run("DPC", 0, 0.000000, 15, NDIS + 0x1234),
    run("DPC", 0, 0.250000, 15, NDIS + 0x1234),
    run("DPC", 2, 0.500000, 15, NDIS + 0x1234),
    run("DPC", 5, 3.250000, 900, AUDIO + 0x2000),
    run("DPC", 0, 4.000000, 15, NDIS + 0x1234),
    image(r"\SystemRoot\System32\drivers\ndis.sys", NDIS, 0x200000, opcode=4, opname="DCEnd", sec=4.1),
    image(r"\SystemRoot\System32\drivers\audiodrv.sys", AUDIO, 0x80000, opcode=4, opname="DCEnd", sec=4.1).replace("0xFFFFF80050000000", "0xfffff80050000000"),
])

# damaged.xml: valid runs mixed with every kind of damage; ends in the middle of an event.
good1 = run("DPC", 0, 0.100000, 40, NDIS + 0x1234)
good2 = run("ISR", 1, 0.200000, 15, STOR + 0x400)
good3 = run("DPC", 0, 0.300000, 60, NDIS + 0x1234)
cut = run("DPC", 0, 0.250000, 99, NDIS + 0x1234)
cut = cut[: cut.index('<Data Name="Routine">') + 25] + "\n"   # line cut off mid-way
unknown_provider = good1.replace(PERF, "{11111111-2222-3333-4444-555555555555}").replace("PerfInfo", "SomethingElse")
sampled_profile = good1.replace("<Opcode>68</Opcode>", "<Opcode>46</Opcode>").replace("<Opcode>DPC</Opcode>", "<Opcode>SampleProf</Opcode>")
negative_initial = run("DPC", 0, 0.400000, 10, NDIS + 0x1234, initial="-5")
huge_initial = run("DPC", 0, 0.410000, 10, NDIS + 0x1234, initial="99999999999999999999999")
max_initial = run("DPC", 0, 0.415000, 10, NDIS + 0x1234, initial="18446744073709551615")
huge_cpu = run("DPC", 0, 0.420000, 10, NDIS + 0x1234).replace('ProcessorID="0"', 'ProcessorID="99999999999999"')
ends_before_start = run("DPC", 0, 0.430000, 10, NDIS + 0x1234, end_text=wall(at(0.429000)))
five_seconds = run("DPC", 0, 0.440000, 5_000_000, NDIS + 0x1234)
year_zero = run("DPC", 0, 0.450000, 10, NDIS + 0x1234, end_text="0000-01-01T00:00:00.000000000+00:59")
no_eventdata = good1.replace("<EventData>", "").replace("</EventData>", "").replace('<Data Name="Routine">', '<Data Name="Routine"')
out_of_order = run("DPC", 0, 0.050000, 7, NDIS + 0x1234)    # finishes before the previous run on CPU 0
huge_image = image(r"\SystemRoot\System32\drivers\big.sys", 0xFFFFF80060000000, 0x10).replace(">0x10<", ">0x1FFFFFFFFFFFFFFFFFFFF<")
tail = run("DPC", 0, 0.900000, 77, NDIS + 0x1234)
tail = tail[: len(tail) // 2]
write("damaged.xml", [header(1.0, events_lost=12, buffers_lost=1)] + kernel_images() + [
    good1, "garbage between events &&& <<<>>> \n", cut, good2, unknown_provider, sampled_profile,
    negative_initial, huge_initial, max_initial, huge_cpu, ends_before_start, five_seconds, year_zero,
    no_eventdata, good3, out_of_order, huge_image, tail,
])
# write() appends </Events>; strip it so the file really ends mid-event.
p = os.path.join(OUT, "damaged.xml")
data = open(p, "rb").read()
open(p, "wb").write(data[: data.rindex(b"</Events>")])

# non_utf8.xml: invalid UTF-8 bytes in a file name and outside any event.
parts = [header(1.0), image(r"\SystemRoot\System32\drivers\ndis.sys", NDIS, 0x200000),
         image("\\SystemRoot\\System32\\drivers\\WEIRDNAME.sys", MOUSE, 0x10000),
         run("DPC", 0, 0.1, 30, MOUSE + 0x10), run("DPC", 1, 0.2, 20, NDIS + 0x10)]
blob = ("<Events>\n" + "".join(parts) + "</Events>\n").replace("\n", "\r\n").encode("utf-8")
blob = blob.replace(b"WEIRDNAME", b"w\xff\xfeird\xc3")
blob = blob.replace(b"<Events>", b"<Events>\r\n\x80\x81 not utf8 \xc0\xc1", 1)
open(os.path.join(OUT, "non_utf8.xml"), "wb").write(blob)
print("FILETIME of trace start:", ft(BASE_UTC))

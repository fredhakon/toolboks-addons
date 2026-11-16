# Test fixtures: synthetic traces

**Every file here is synthetic.** None of them was recorded on a real PC. They were written by
`generate.py` (run `python generate.py .` in this folder to rebuild them) to look like the XML
that Windows' `tracerpt trace.etl -o trace.xml -of XML` writes for an NT Kernel Logger trace
with DPC, interrupt and image-load events.

What the shape is based on:

- The element layout (`<Events>` wrapper, `System`, `EventData`, `RenderingInfo`,
  `ExtendedTracingInfo/EventGuid`), space-padded numbers, CRLF line endings and the header
  event's fields were copied from real tracerpt output of a small non-kernel trace converted
  on a Windows 11 PC during development. That run also showed `TimeCreated` written as local
  wall-clock time with a wrong-looking `+00:59` suffix, which the fixtures reproduce.
- The DPC, ISR and Image event fields and type numbers follow Microsoft's MOF class
  documentation (links in the crate docs, `src/lib.rs`).
- `InitialTime` is written as a FILETIME number, as shown in a real tracerpt sample quoted in
  <https://github.com/microsoft/perfview/issues/234>.

Not verified: no real DPC/ISR kernel trace has been run through this crate yet. The first
real recording may show differences (for example in how `InitialTime` or `Routine` is printed).

| File | What it covers |
|---|---|
| `basic.xml` | One run per driver, a clear ranking, a user-mode DLL (ignored) and a driver loaded from a user folder (only its file name may appear). |
| `nested.xml` | An ISR inside a DPC on the same CPU, a run on another CPU at the same moment (not nested), and three levels (threaded DPC, DPC, ISR). |
| `unresolved.xml` | Runs whose routine is in no image, including a null routine. |
| `spike.xml` | No header (the clock offset is learned from the runs), images only in the end-of-trace rundown, one short spike on CPU 5. |
| `damaged.xml` | Good runs mixed with a cut-off event, garbage, unknown events, negative, huge and out-of-range numbers, impossible timing, an out-of-order run, and a file that ends mid-event. |
| `non_utf8.xml` | Invalid UTF-8 bytes in a file name and outside any event. |

Timeline used by all files: the trace starts at 2026-11-16 09:00:00 UTC (FILETIME
134392932000000000) and the wall clock is UTC + 1 hour.

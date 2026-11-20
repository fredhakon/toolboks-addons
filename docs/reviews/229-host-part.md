# Issue 229: what the host may need to change

The add-on side is finished and works without a host change if the host passes the request on
as it does today. Two things depend on the host, and neither was checked here because the host
is not in this repository.

1. Operation name and payload. "Clean selected regions" reuses the existing operation name
   `purge_standby` and puts the choice in the request's `json`, as `{"regions": <mask>}`
   (bit 0 is region 1, bit 7 is region 8). The old purge still sends `{}`. The add-on's own
   `prepare_admin` validates the payload and refuses an empty, out of range or unavailable mask.
   If the host only lets through `json` equal to `{}` for this add-on, or checks the payload
   against a fixed schema, it must accept an object with a `regions` number.
2. Confirmation window text. The window text now lists each selected region with its note and a
   freeze warning, about 2,500 characters when all eight are ticked. The host's window
   must show that much text (scrolling or a taller window) without cutting it, because the
   user confirms based on it. If the host has a smaller limit than the add-on's existing 8,000
   character check, the add-on text must be shortened instead; tell the add-on owner.

No new capability is needed: the same `system_tuning` capability applies.

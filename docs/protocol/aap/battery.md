# AAP battery

The battery packet (`0x04`): one entry per part, each a component, a level and a
status. The layout and the codes are LibrePods', and AirPods Pro 3 captures (#71, #108,
#142) agree with them. The order of the entries and the placeholder levels come from
those captures alone; of the Pro 2 (USB-C), only the order at a role switch was seen
(#71).

Read by `aap::battery`; what the app does with the levels: [app/battery.md](../../app/battery.md).

Byte offsets count from 0 at the first byte of the packet.

## The packet

```
Offset:  0  1  2  3  4  5  6   7  8  9  10 11   12 ...
Hex:     04 00 04 00 04 00 NN  CC 01 LL SS 01   [next entry] ...
         ~~~~~~~~~~~ ~~    ~~  ~~~~~~~~~~~~~~
         Header      Opcode Count  Entry, 5 bytes
```

| Bytes | Field | Notes |
|---|---|---|
| 0-3 | Header | `04 00 04 00`, as every packet after the handshake ([session.md](session.md#packets)) |
| 4 | Opcode | `04` |
| 6 | Count | The number of entries. `03` in every packet captured here. |
| 7 on | Entries | 5 bytes each: component, `01`, level, status, `01`. |

The layout is from [LibrePods' AAP Definitions](https://github.com/librepods-org/librepods/blob/main/docs/AAP%20Definitions.md)
("Battery"). The two `01` bytes of an entry were `01` in every packet captured here;
what they mean is not known.

The pods send the packet unasked:

- at link-up, shortly after the notification request ([session.md](session.md#initial-state-report));
- at a role switch, in the new order, and at a lid change (Pro 3, #142 and #71
  capture 4; Pro 2 (USB-C), #71): see [ear.md](ear.md#the-role-switch).

LibrePods' own byte table labels the first entry's component "Left", against its
component codes, where `02` is the right pod. This document reads the entries by the
codes.

LibrePods' example, from an AirPods Pro 2: `04 00 04 00 04 00 03 02 01 64 02 01 04 01 63 01 01 08 01 11 02 01`
is the right pod at 100% discharging, the left at 99% charging, and the case at 17%
discharging.

## Components

| Byte | Part |
|---|---|
| `02` | Right pod |
| `04` | Left pod |
| `08` | Case |

From LibrePods. A pod's code agreed with the primary pod packet in every #142 log (Pro 3,
2026-11-01): `03 04 …` came with `08 01` (left primary), `03 02 …` with `08 02` (right).

## Status

| Byte | Status | Seen here |
|---|---|---|
| `00` | Unknown (LibrePods) | Never. |
| `01` | Charging | A pod in the case. It reported `01` even at 100% (Pro 3, #71 capture 1). |
| `02` | Discharging | A pod out of the case. |
| `04` | Disconnected | A part that is not reporting: see [Disconnected is no 0%](#disconnected-is-no-0). |
| `05` | Not in LibrePods' table | A pod in the open case at 96-100%, again and again (Pro 3, 2026-11-10); once with the pods in the open case in the traces of PR #109 (Pro 3, #108). Its meaning is not known: [Status `05`](#status-05). |

The names of `00`-`04` are LibrePods'.

### Status `05`

In the 2026-11-10 run (Pro 3; [Runs](#the-2026-11-10-run-pro-3)), a pod in the open case
reported `05`, packet after packet:

- the right pod at 100%, from 18:56:25 on, while the left pod was worn or in the hand;
- both pods, the left at 99% and the right at 100%, from 19:01:14 on;
- after the link came back with both pods in the open case, the right pod at 100% from
  the first packet, and the left pod at 97% and then 96% from 57 ms after its first
  level.

The same place and level also gave `01`, Charging: the left pod at 100% in the open
case, with the right pod worn, in each packet at 18:51:29 and 18:54:00. So `05` does
not come from the level alone. The right pod reported `01` once, 0.9 s before its first
`05`; after the link came back, the left pod's first level came with `02`, 57 ms before
its `05`.

While both pods reported `05`, their advertisements set both charging bits (`b5`;
[ble/proximity-pairing.md](../ble/proximity-pairing.md#byte-5-charging-status)).

## Level

The level is a percentage, `00`-`64` (100): LibrePods' example has `64` for 100% and
`63` for 99%, and the captures here have `64` from full pods.

### Disconnected is no 0%

With status `04` the level byte is a placeholder, not a reading. It came as `00` as well
as `FF`, so a level of `00` beside status `04` is no empty battery (#108):

| Situation | Pod in the case | Case | Source |
|---|---|---|---|
| Both pods worn, the case away | - | level `00`, status `04` | Pro 3, #71 capture 1 (#108) |
| One pod worn, the other in the open case | level `64`, status `01` | level `FF`, status `04` | Pro 3, #71 capture 1 (#108) |
| One pod worn, the other in the closed case | level `00`, status `04` | status `04` | Pro 3, #142 capture 2 (the pod) and #71 capture 4 (both) |

While a pod is in the closed case, neither its level nor the case's is available, over
AAP or over BLE (Pro 3, #71 capture 4).

## Order of the entries

The primary pod comes first, then the other pod, then the case. The order agreed with
the primary pod packet (`0x08`) in every #142 log (Pro 3, 2026-11-01), and at every role
switch the battery came again in the new order (Pro 3, #142; Pro 2 (USB-C), #71,
2026-11-02): see [ear.md](ear.md#the-role-switch).

#71 capture 2 (Pro 3) listed left, right, case where capture 1 had listed right, left,
case, and read the change as following the pod AAP had reached: the `0x1D` model number
changed between the two captures too. A role switch reorders the entries on one link,
which that reading does not explain.

## Runs

### #108 and #71 capture 1 (Pro 3)

#108's first packet, captured on a Pro 3 by 2026-10-20 (`btmon`, PR #109): the case
away, both pods worn, the left one listed first.

```
04 00 04 00 04 00 03  04 01 4e 02 01  02 01 4f 02 01  08 01 00 04 01
                      left 78%        right 79%       case: level 00, status 04
```

#71 capture 1 (`btmon`, `dbus-monitor --system "sender='org.bluez'"`, the app at
`RUST_LOG=linuxpods=trace`, build 06a426d, posted 2026-10-25):

```
04 00 04 00 04 00 03  02 01 64 02 01  04 01 64 02 01  08 01 00 04 01   both worn, the case away
04 00 04 00 04 00 03  02 01 64 02 01  04 01 64 01 01  08 01 ff 04 01   left in the open case, right worn
```

### One pod in the closed case (Pro 3)

#142 capture 2 (2026-11-01, a local build of df17831 logging up to 64 bytes of each
packet): with the secondary pod in the closed case, its entry read `04 01 00 04 …`,
status `04` and level `00`. #71 capture 4 (2026-11-01, build df29b14,
`RUST_LOG=linuxpods=trace`): about 4.5 s after the lid closed on one pod, the battery
reported that pod and the case as Disconnected; with the lid open again, both came back.

### The 2026-11-10 run (Pro 3)

AirPods Pro 3, firmware 9A348: the run of
[ble/proximity-pairing.md](../ble/proximity-pairing.md#the-2026-11-10-run), at
`RUST_LOG=cymba=trace`. Its battery packets, with where the pods were per the ear
report ([ear.md](ear.md#ear-report-0x06)):

```
18:51:29  04 00 04 00 04 00 03  02 01 64 02 01  04 01 64 01 01  08 01 ff 04 01   left in the open case
18:54:00  04 00 04 00 04 00 03  02 01 64 02 01  04 01 64 01 01  08 01 3a 02 01   left in the open case
18:56:24  04 00 04 00 04 00 03  02 01 64 01 01  04 01 64 02 01  08 01 ff 04 01   right just in the open case
18:56:25  04 00 04 00 04 00 03  04 01 64 02 01  02 01 64 05 01  08 01 3a 02 01   right in the open case, left worn
19:00:54  04 00 04 00 04 00 03  04 01 63 02 01  02 01 64 05 01  08 01 3a 02 01   right in the open case, left in the hand
19:01:14  04 00 04 00 04 00 03  04 01 63 05 01  02 01 64 05 01  08 01 3a 02 01   both in the open case
19:03:37  04 00 04 00 04 00 03  02 01 64 05 01  04 01 00 04 01  08 01 3a 02 01   the link back, the left pod in the closed case per the ear report
19:03:41  04 00 04 00 04 00 03  02 01 64 05 01  04 01 61 02 01  08 01 3a 02 01   both in the open case
19:03:41  04 00 04 00 04 00 03  02 01 64 05 01  04 01 61 05 01  08 01 3a 02 01   57 ms later
19:04:35  04 00 04 00 04 00 03  02 01 64 05 01  04 01 60 05 01  08 01 3a 02 01   both in the open case
19:04:48  04 00 04 00 04 00 03  02 01 64 02 01  04 01 60 02 01  08 01 00 04 01   both out
```

The packets at 18:58:54 and 19:00:10 are those of 18:56:25 again.

## Open questions

- What status `05` means. A pod in the open case at 96-100% reported it, and in other
  packets `01` in the same place and at the same level ([Status `05`](#status-05)).
- What the two `01` bytes of each entry mean.
- When else a battery packet comes: at every level change, at a change of charging?
- A level above `64` other than `FF`, or `FF` beside a status other than `04`: never seen.
- A count other than 3: products with no case, or one battery, are untried.
- Pro 2 (USB-C): the placeholders and status `05` are assumed to be as on the Pro 3, not
  checked.

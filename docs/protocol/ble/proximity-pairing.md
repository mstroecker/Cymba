# BLE proximity pairing

The proximity pairing advertisement AirPods broadcast over BLE: its cleartext bytes,
its 16 encrypted bytes, and how they decrypt and validate. The layout started from
LibrePods' parser and has been checked here on two models only, AirPods Pro 3 and
AirPods Pro 2 (USB-C) (see [Runs](#runs)). Rows for other models come
from public tables and are unverified; several bytes are still unexplained (see
[Open questions](#open-questions)).

Parsed by `ble::parser` and `ble::decrypt`; what Cymba does with it:
[app/identification.md](../../app/identification.md).

**Byte offsets** count from the first payload byte, the one after the type and length
bytes: byte 0 is the prefix `0x01`. In the decrypted block, they count from its first
byte.

## Overview

AirPods broadcast their batteries, charging, in-ear, lid and connection state in BLE
advertisements, which any receiver nearby can read without a connection.

| Part | Bytes | What it holds | Needs |
|---|---|---|---|
| Cleartext | 0-8 | Model, status, lid, connection state, batteries in 10% steps | Nothing |
| Encrypted | 9-24 | Exact batteries, the last three bytes of the pair's real MAC | The pair's ENC_KEY, sent over AAP ([aap/keys.md](../aap/keys.md)) |

- **Identifying the pair needs both keys.** An advertisement comes from a resolvable
  private address that only the pair's IRK resolves ([addresses.md](addresses.md)),
  and only the pair's ENC_KEY decrypts it. Both keys come over AAP. What Cymba
  counts, and what it drops:
  [app/identification.md](../../app/identification.md#what-counts).
- **The payload changes seldom.** It changes when something happens, and a few times
  in the minute after; otherwise it stays the same for minutes
  ([Update Frequency](#update-frequency)).

## Advertisement Structure

### Manufacturer Data Format

The advertisement is Apple's entry in the manufacturer data: company ID `0x004C`,
Apple Inc. (Bluetooth SIG Assigned Numbers). Its value is:

| Offset | Field | Value |
|---|---|---|
| 0 | Message type | `0x07`, proximity pairing (furiousMAC/continuity, `messages/proximity_pairing.md`) |
| 1 | Payload length | `0x19` (25) in every payload seen here, on both models (7ba6bbc, 2026-09-09) |
| 2- | Payload | Byte 0 below |

### Payload Structure

| Byte | Field | Example | Details |
|---|---|---|---|
| 0 | Prefix | `0x01` | `0x01` in every payload captured here, on both models (`examples/decrypt_probe.rs`, #71) |
| 1-2 | Product ID, little-endian | `27 20` | [Bytes 1-2](#bytes-1-2-product-id) |
| 3 | Status | `0x0b` | [Byte 3](#byte-3-status-byte) |
| 4 | Pod batteries, a nibble each | `0x88` | [Byte 4](#byte-4-battery-levels-leftright-airpods) |
| 5 | Charging bits, case battery nibble | `0x07` | [Byte 5](#byte-5-charging-status) |
| 6 | Lid | `0x31` | [Byte 6](#byte-6-lid-state) |
| 7 | Color | `0x00` | [Byte 7](#byte-7-device-color) |
| 8 | Connection state | `0x04` | [Byte 8](#byte-8-connection-state) |
| 9-24 | Encrypted: exact batteries, MAC suffix | | [Bytes 9-24](#bytes-9-24-encrypted-battery-data) |

## Byte-by-Byte Parsing

### Which pod sent it

Each pod advertises as itself, from an address of its own, and calls itself "primary"
in its own advertisement (Pro 3, 2026-10-25, #71). Status bit 5 therefore names the
pod that sent it: `1` the left pod, `0` the right one. Its agreement with the primary
pod AAP reports is under [Update Frequency](#update-frequency).

The fields that carry one value per pod name the pods relative to the sender, so the
same bit means the left pod in one advertisement and the right pod in the next:

| Field | Left pod sends (bit 5 = 1) | Right pod sends (bit 5 = 0) | Source |
|---|---|---|---|
| Byte 4, low nibble | Left | Right | Pro 3, 2026-11-07, #184; LibrePods' Linux parser; furiousMAC/continuity |
| Byte 4, high nibble | Right | Left | As the low nibble |
| Byte 5, bit 4 (charging) | Left | Right | LibrePods' Linux parser; Pro 3, 2026-11-10 |
| Byte 5, bit 5 (charging) | Right | Left | LibrePods' Linux parser; Pro 3, 2026-11-10 |
| Status bit 1 (in ear), sender out of the case (bit 6 = 0) | Left | Right | LibrePods' Linux parser |
| Status bit 3 (in ear), sender out of the case (bit 6 = 0) | Right | Left | LibrePods' Linux parser |
| Status bit 1 (in ear), sender in the case (bit 6 = 1) | Right | Left | LibrePods' Linux parser |
| Status bit 3 (in ear), sender in the case (bit 6 = 1) | Left | Right | LibrePods' Linux parser |
| Decrypted byte 1 | Left | Right | Pro 3, 2026-10-25, #71 |
| Decrypted byte 2 | Right | Left | Pro 3, 2026-10-25, #71 |

In short: the sender has the low battery nibble, charging bit 4 and decrypted byte 1.
The case's fields do not depend on the sender.

- **Byte 4:** with the left pod at 56-57% and the right at 80%, each pod sent its own
  level in the low nibble, the left pod as well as the right (Pro 3, 2026-11-07, #184;
  [Runs](#the-184-run)). furiousMAC/continuity gives "the 4 MSBits are the right
  battery and the 4 LSbits are the left battery", which is the left pod's order. The
  September sample of the right pod sending agrees: byte 4 = `0x78` beside the
  decrypted levels 80% for itself and 77% for the left pod (Pro 3, 2026-09-11,
  17f6591; [Runs](#september-samples)).
- **Decrypted bytes 1 and 2:** in the first #71 capture every level was 100%, but the
  charging bits settled it: in each advertisement from the right pod, decrypted byte 1
  was the right pod and byte 2 the left (Pro 3, 2026-10-25, #71).

### Bytes 1-2: Product ID

Apple's product ID, little-endian. It is the number BlueZ's `Modalias` carries for the
same pair: `p2027` for AirPods Pro 3, `p2024` for AirPods Pro 2 (USB-C) (2a4003d,
2026-10-16). Read big-endian, the bytes `27 20` give `0x2720`, a second number for one
model; LibrePods' table and furiousMAC/continuity's dissector list the IDs that way.

| Product ID | Model | Source |
|---|---|---|
| `0x2002` | AirPods (1st gen) | LibrePods and AirPodsDesktop agree |
| `0x200F` | AirPods (2nd gen) | LibrePods and AirPodsDesktop agree |
| `0x2013` | AirPods (3rd gen) | LibrePods and AirPodsDesktop agree |
| `0x2019` | AirPods 4 | LibrePods and AirPodsDesktop agree |
| `0x201B` | AirPods 4 (ANC) | LibrePods and AirPodsDesktop agree |
| `0x200E` | AirPods Pro | LibrePods and AirPodsDesktop agree |
| `0x2014` | AirPods Pro (2nd gen) | LibrePods and AirPodsDesktop agree |
| `0x2024` | AirPods Pro (2nd gen, USB-C) | Verified here |
| `0x2027` | AirPods Pro 3 | Verified here |
| `0x200A` | AirPods Max | LibrePods and AirPodsDesktop agree |
| `0x201F` | AirPods Max (USB-C) | LibrePods and AirPodsDesktop agree |

The tables are LibrePods' `linux/ble/blemanager.cpp` and AirPodsDesktop's
`Source/Core/AppleCP.cpp` (see [References](#references)); a model is listed only
where both agree (2a4003d). furiousMAC/continuity's dissector lists the first two
generations and the first AirPods Pro.

### Byte 3: Status Byte

The status byte says which pod sent the advertisement, which pods are in an ear, and
which are in the case.

| Bit | Meaning | Source |
|---|---|---|
| 0 | Unknown; with an AAP link up, it matched `0x08`'s `SS`: below | Pro 3, 2026-11-10; Pro 3, 2026-11-01, #71 capture 4 and #142 session 2 |
| 1 | A pod is in an ear; which one: [Which pod sent it](#which-pod-sent-it) | LibrePods' Linux parser |
| 2 | Both pods are in the case; exception below | LibrePods' Linux parser; Pro 3, 2026-10-25, #71 |
| 3 | The other pod's in-ear bit; see bit 1 | LibrePods' Linux parser |
| 4 | A pod is in the open case, set by both pods; exceptions below | Pro 3, 2026-11-10; Pro 3, 2026-10-25, #71; Pro 3, 2026-11-07, #184; LibrePods' Linux parser names the bit |
| 5 | The sending pod: `1` left, `0` right | LibrePods' Linux parser; Pro 3, #71 and #142 |
| 6 | The sending pod is in the case | LibrePods' Linux parser; Pro 3, 2026-10-25, #71 |
| 7 | Unknown; clear in every advertisement here | Pro 3 and Pro 2 (USB-C), 2026-10-28 to 2026-11-10 |

- **Bit 0:** with an AAP link up, it matched the `SS` byte of the last primary pod
  packet (`0x08`, [aap/ear.md](../aap/ear.md#primary-pod-0x08)) in every advertisement
  from either pod but two: clear while `SS` was `00`, set while it was `01` (Pro 3,
  2026-11-10, #71 capture 4 and #142 session 2). The two came in #142 session 2, set
  0.22 s before an `SS` of `01` arrived (17:23:17.45). The Pro 2 (USB-C) role switch run
  (2026-11-02) had `SS` `01` and bit 0 set throughout. Across every log here, bit 0 was
  clear only in `0x00`, `0x02`, `0x04`, `0x08`, `0x14`, `0x20`, `0x22`, `0x24` and
  `0x34` from the Pro 3, and in `0x00`, `0x04`, `0x14` and `0x24` from the Pro 2 (USB-C).
- **Bit 7** was clear in every advertisement of the app logs from 2026-10-28 to
  2026-11-10: 771 distinct ones from the Pro 3 and 100 from the Pro 2 (USB-C)
  ([Runs](#runs)).

What the Pro 3 showed of bits 2, 4 and 6 in the #71 captures (2026-10-25), the #184
run (2026-11-07) and the 2026-11-10 run, and of bit 6 in the other app logs
([Runs](#runs)):

- **Bit 2:** with both pods in the case, every advertisement from either pod has it.
  A pod in the case also sets it, with bits 4 and 6 clear, while its partner is out:
  - in its first advertisements after it goes into the case or after the lid opens on
    it: `0x24`, `0x04` and `0x0d` (#184; 2026-11-10);
  - once as its partner was taken out: `0x05`, followed 74 ms later by `0x01`, both
    pods out (2026-11-10).

  What it means there is an [open question](#open-questions).
- **Bit 6:** with one pod in the case and the other out, the pod in the case sets it,
  but for the advertisements under bit 2 above. With both inside, at most one pod at a
  time set it. With both inside and an AAP link up, the pod that set it was the one the
  link's `0x08` named secondary: each time on the Pro 3 (the PR #137 check at 23:47:55
  and 23:59:53, #71 capture 4 at 19:40:19 and 19:43:19, 2026-11-10 at 19:01:14 and
  19:03:41) and once on the Pro 2 (USB-C) (#62 first pairing, 17:38:39). Once a pod
  out of the case set it: the left pod's `0x69`, out of the ear, 2 ms before AAP's
  `0x08` named the right pod primary (#71 capture 4, 19:39:57.685).
- **Out of the case or in ears,** neither bit 2 nor bit 6 is set, but for the one `0x69`
  under bit 6.
- **Bit 4:** both pods set it while a pod is in the open case, whether the other pod is
  worn, in the hand or in the case too: `0x73` beside `0x13`, `0x51` beside `0x31`,
  `0x35` beside `0x55` (2026-11-10). It is clear:
  - with no pod in the case, the case open or closed: `0x0b` both worn, `0x01` both on
    the desk;
  - from the worn pod, with its partner in the closed case: `0x02` and `0x22`. The pod
    in the closed case kept bit 4 in its last advertisements before it fell silent
    (`0x73` with byte 6 `0x39`);
  - in the advertisements of a pod in the case under bit 2 above. After the first ones
    (`0x24`, `0x04`, `0x0d`), AAP's primary pod packet and ear report came 0.1-12 s
    later ([aap/ear.md](../aap/ear.md)); that pod then sent bits 4 and 6 (`0x73`,
    `0x53`), and its partner bit 4 (`0x13`, `0x33`).

  Two kinds of advertisement do not fit ([Open questions](#open-questions)): #71's
  `0x29`, `0x22` and `0x20`, from the left pod in the hand while the right pod sent
  `0x51` from the open case; and #71's `0x04`, from the right pod after the lid opened
  on both pods in the case.
- Bits 2 and 6 together gate the lid: see [Byte 6](#byte-6-lid-state).

#### Ear Detection

Bits 1 and 3 say which pods are in an ear, read as in
[Which pod sent it](#which-pod-sent-it). Each pod advertises its own status byte, so
two advertisements can disagree: with both pods worn, they flipped between left
out/right in and left in/right out within 0.6 s (Pro 3, 2026-11-01, #142). Where
Cymba takes in-ear from instead: [app/in-ear.md](../../app/in-ear.md).

### Byte 4: Battery Levels (Left/Right AirPods)

One nibble per pod: on the Pro 3, the sender's in the low nibble, the other pod's in the
high one ([Which pod sent it](#which-pod-sent-it)); on the Pro 2 (USB-C), an
[open question](#open-questions). Each nibble is a level in 10% steps,
rounded down (see [Batteries in the clear and encrypted](#batteries-in-the-clear-and-encrypted)).

#### The other pod's fields

**A pod sends the other pod's level as `f`, unknown, until the two pods have exchanged
state, and the case's level as `f` while no pod is in the case and at times beside
one** (Pro 3, 2026-11-10; [Runs](#the-2026-11-10-run)). Its own level is there from its
first advertisement.

| Field | `f` while | Samples |
|---|---|---|
| Byte 4, the other pod's level (high nibble) | The other pod is in the closed case | `02 fa`, `22 fa` |
| | In a pod's first advertisements after a change | `24 fa`, `04 fa`; the worn right pod's `13 fa`, then `13 aa` 0.3 s later; the worn left pod's `33 fa` |
| | As the run began, before the AAP link | `00 fa`, `02 fa`, then `03 aa` |
| Byte 5, the case level (low nibble) | No pod is in the case | `8f` with both pods worn, byte 4 already `aa`, and from the first advertisement after both pods came out |
| | From the worn pod, the other just put in the open case | The worn right pod's `af`, unchanged until the lid closed 70 s later; the worn left pod sent `a5` at once in the same place |
| | From the worn pod just after the lid opened on the other | `af`, then `a5` 0.3 s and 0.5 s later |

#184's `24 f5` is the same: the left pod in the case, around the lid opening, sent `f`
for the right pod worn at 80% ([The #184 run](#the-184-run)).

### Byte 5: Charging Status

| Bit | Meaning |
|---|---|
| 0-3 | Case battery, a nibble as in byte 4; `0xF` unknown |
| 4 | The sending pod is charging |
| 5 | The other pod is charging |
| 6 | The case is charging |
| 7 | Unknown; set in every byte 5 captured here |

The layout is LibrePods' Linux parser's; the #71 captures (Pro 3, 2026-10-25) and the
2026-11-10 run agree with it. When the case level reads `f`:
[The other pod's fields](#the-other-pods-fields).

**The charging bits follow the pods** (Pro 3, 2026-11-10; [Runs](#the-2026-11-10-run)):

| Where the pods were | Byte 5 |
|---|---|
| Both in the case | `b5` from either pod: both charging bits set, the case not charging, the case at 50% |
| One pod in the case | `95` from it, `a5` or `af` from the other: only the pod in the case charging |
| Both out of the case | `8f`: neither charging |

One advertisement did not fit: the right pod's `05 9a b5` from the open case, both
charging bits set, 0.2 s after AAP's ear report put the left pod out
([aap/ear.md](../aap/ear.md)). Its next advertisement, 74 ms later, was `01` with `8f`.

**Bit 7** was set in every advertisement of the 2026-11-10 run (`8f`, `95`, `a5`,
`af`, `b5`), in the #184 run's (`8f`, `98`, `a8`), in every one of the app logs of
2026-10-28 to 2026-11-10 on both models (`96`, `b6`, `d5`, `e5`, `f5` among them from the
Pro 3, `90` and `b0` from the Pro 2 (USB-C); [Runs](#runs)), and in the September
captures of `examples/decrypt_probe.rs`: `f3`, `f5` and `8f` from the Pro 3, `f1` from
the Pro 2 (USB-C).

### Byte 6: Lid State

| Bits | Meaning | Source |
|---|---|---|
| 0-2 | Unknown; not a lid-open counter | Pro 3, 2026-10-25, #71 |
| 3 | Lid: `0` open, `1` closed, while a pod is in the case | LibrePods' Linux parser; Pro 3, 2026-10-25, #71 |
| 4-7 | Follow the pod and the case's charging; values and exceptions below | Pro 3, 2026-11-01, #71 |

**Bit 3 is the lid only while a pod is in the case,** that is while status bit 6 or bit
2 is set. With the pods out, bit 3 reads 0 whatever the lid is doing. Read that way,
the lid came out right for about 9,000 advertisements across the two #71 captures but
one, a right-pod `0x31` 45 ms before the lid closed (Pro 3, 2026-10-25, #71, #114).
With one pod worn and the other in the open case, both advertise: the one in the case
reads the lid (`0x73`), the worn one does not (`0x13`).

**Closing the lid flips bit 3 and nothing else** in bytes 0-8: both pods went from
`0x31` (the right pod from `0x11`) to `0x39` within 22 ms (Pro 3, 2026-10-25, #71).
The pods then stop advertising ([Update Frequency](#update-frequency)), so a closed
lid is seen only in the last advertisements before they go silent.

**The high nibble** follows the pod and the case's charging, but for the exceptions
below (Pro 3, 2026-11-01, #71):

| High nibble | Sender |
|---|---|
| `0x1` | Out of the case, whether the case charges or not |
| `0x3` | In a case that is not charging |
| `0x5` | In a charging case |

Bit 4 was set in every byte 6 captured here, on both models.

**`0x1` from a pod in the open case,** not the `0x3` of the table (Pro 3):

- **After the lid opened** on it, a pod in the case sent `0x1` in every advertisement:
  - 2026-11-10: the left pod 18:53:48-18:54:00, its last advertisements heard before
    it was taken out; the right pod 18:58:44-19:01:12, then `0x3` from 19:01:14, once
    the left pod was in beside it; both pods 19:03:34-19:04:48, 74 s, until they were
    taken out.
  - #71 capture 4: the left pod after the lid opened at 19:41:44, until 19:41:46.2,
    then `0x3` 0.5 s after the right pod's first advertisement from out of the case;
    the left pod again from 19:43:15, then `0x3` 0.4 s after AAP's ear report put the
    right pod in the case beside it (19:43:19.9).
  - The PR #137 check: the right pod at 23:48:13, until it came out 1.9 s later.

  Other pods sent `0x3` from their first advertisement after the lid opened: the right
  pod at 19:43:32 in #71 capture 4, the left pod at 23:48:14 in the PR #137 check.
- **Going into an open case,** a pod sent `0x1` for 0.1-0.3 s first, then `0x3`:
  2026-11-10 at 18:51:29 and 19:01:14; #71 capture 4 at 19:40:17, 19:40:19, 19:42:50,
  19:43:19, 19:46:02 and 19:46:04; the PR #137 check at 23:47:53, 23:47:55, 23:59:48
  and 23:59:51. Others sent `0x3` from the first: the right pod at 18:56:24 on 2026-11-10
  and at 19:42:25 in #71 capture 4, both pods in the #62 rename check (16:45:45-46).

**`0x5` with byte 5's case-charging bit clear:** in #142 session 2 (Pro 3, 2026-11-01,
17:22-17:24), every advertisement from a pod in the case had `0x5` but the left pod's
first, `0x34`, 0.4 s before its `0x54`. Byte 5 was `96` or `b6`, and the decrypted case
level 67-68% without its charging bit. The Pro 2 (USB-C), both pods in the case just
opened, the case at 1% and not charging (byte 5 `b0`), sent `0x11`, then `0x71` from
0.7 s after its first advertisement, and `0x51` in the next one heard, 13 s later, once
the AAP link was up (#62 first pairing, 2026-11-01).

**The low bits are not a lid-open counter,** which LibrePods calls them: they stayed
`1` across two lid opens and went to `2` when the pods went back in (Pro 3, 2026-10-25,
#71). The captures behind these rules: [Runs](#the-71-captures). In the app logs of
2026-10-28 to 2026-11-10 ([Runs](#the-app-logs-of-2026-10-28-to-2026-11-02)):

- **Each pod has its own:** with both in the case, `0x32` beside `0x31` and `0x3a`
  beside `0x39`.
- **Values `0` to `4`** came from the Pro 3, and only `0` and `1` from the Pro 2 (USB-C).
- **Going into the case,** a pod's value went up by one, `0` to `1`, `1` to `2`, `2`
  to `3` or `3` to `4`, within 0.2 s of its first advertisement from the case: 18 of
  the 19 dockings whose value before was heard, 0.1-61 s before. In the 19th it went
  from `0` to `2`, the pod unheard for the 40 s before (#62 rename check, 16:45:45).
- **After the lid had been closed for 12-71 s,** every pod heard after it opened sent
  `1`, whatever its value before (11 openings, 17 pods). Twice a pod sent `0` first,
  for 0.45 s (#71 capture 4, 19:47:03) and 1.1 s (#142 session 2, 17:23:16).
- **After the lid had been closed for 5 s,** each pod's value had gone up by one:
  `0x39` to `0x32` and `0x3a` to `0x33` (#62 rename check, 16:45:47-16:45:52).
- They changed at no other time.

How Cymba reads and keeps the lid: [app/state.md](../../app/state.md#the-lid).

### Byte 7: Device Color

| Value | Color |
|---|---|
| `0x00` | White |
| `0x01` | Black |
| `0x02` | Red |
| `0x03` | Blue |
| `0x04` | Pink |
| `0x05` | Gray |
| `0x06` | Silver |
| `0x07` | Gold |
| `0x08` | Rose Gold |
| `0x09` | Space Gray |
| `0x0A` | Dark Blue |
| `0x0B` | Light Blue |
| `0x0C` | Yellow |

The table is LibrePods' Linux parser's. Every captured advertisement in
`examples/decrypt_probe.rs`, from both models verified here, carries `0x00`, White. So
does every advertisement of the app logs from 2026-10-28 to 2026-11-10: 771 distinct
ones from the Pro 3 and 100 from the Pro 2 (USB-C), in and out of the case, the lid open
and closed, charging or not, connected to this host, to another host or to none
([Runs](#runs)). The rest is unverified.

How Cymba shows it: [app/ui.md](../../app/ui.md#model-and-color).

### Byte 8: Connection State

Whether a host is connected, and what the AirPods are doing on it. The values and
names are LibrePods' Linux parser's (`linux/ble/blemanager.h`):

| Value | State |
|---|---|
| `0x00` | Disconnected |
| `0x04` | Idle |
| `0x05` | Music |
| `0x06` | Call |
| `0x07` | Ringing |
| `0x09` | Hanging Up |
| `0xFF` | Unknown: LibrePods' own stand-in for no value, "representing null in the original" |

Some values come and go (Pro 3, 2026-10-25, #71):

- `0x08` for an advertisement or two while a connection is set up, before `0x04`. The
  first capture had `0x0c` there.
- `0x04` for one or two advertisements while the pods page the host they last used,
  after the lid opens or the pods leave the case, even when the page is rejected and
  nothing connects.

In the app logs of 2026-10-28 to 2026-11-10 ([Runs](#runs)), byte 8 was `0x00`,
`0x04`, `0x05`, `0x08` or `0x0c`:

- **`0x05`** came 0.38 s and 81 ms after AAP's `0x0E` named this host with type `02`
  ([aap/session.md](../aap/session.md#0x0e-the-audio-source)), and stayed while the link
  was up, the pods in the case or out of it (Pro 3, 2026-11-10, 18:48:14.9 and
  19:03:41.8). The exceptions were `0x00`, each from a pod just in the case: its `0x24`
  and `0x04` after the lid opened on it ([The 2026-11-10 run](#the-2026-11-10-run)),
  and the right pod's `0x51` at 18:56:24.984. In #71 capture 4 every `0x0E` had type
  `00`, and byte 8 was `0x04` while this host was connected, with the same exceptions
  (19:42:25, 19:42:43, 19:43:15). With the iPhone connected, it went between `0x04` and
  `0x05` (#71 capture 4, 19:44:26-19:45:24).
- **`0x0c`** came in four single advertisements, each followed 49-311 ms later by one
  whose decrypted bytes 4-6 named another host: Pro 3, #71 capture 4 (19:44:02.220) and
  the #62 rename check (16:43:23.839, 16:43:29.268); Pro 2 (USB-C), 2026-11-02
  (19:26:25.372). Decrypted byte 10's high nibble was `f` in these four.
- **`0x08`** came in three advertisements, at most two in a row, in the #64 check
  (Pro 3, 2026-11-01, 16:34:59-16:35:00): the pods just out of the case, while
  decrypted bytes 4-6 went from another host to the iPhone and back, before this host
  connected at 16:35:06. Decrypted byte 10 was `0f` in one and `ff` in two; no other
  advertisement had byte 10's high nibble `f`.

⚠️ **Byte 8 is the last cleartext byte;** everything from byte 9 on is encrypted. A
parser reading "connection state" from byte 9 reads ciphertext, and its output is
meaningless (3eb67a6, 2026-09-11). Take that field from the decrypted payload instead,
or treat it as unknown.

### Bytes 9-24: Encrypted Battery Data

The last 16 bytes hold the exact batteries, and the real MAC's suffix, which confirms
that the key was the pair's.

- **Algorithm:** AES-128 in ECB mode: a single block, no IV, no padding.
- **Key:** the ENC_KEY (key type `0x04`), which the pair sends over its AAP connection
  (L2CAP PSM 4097): [aap/keys.md](../aap/keys.md).
- **Where it is:** every payload seen here is exactly 25 bytes, so bytes 9-24 are also
  the last 16 bytes (7ba6bbc, 2026-09-09). Where it sits in a payload of another length
  is an [open question](#open-questions). Which range Cymba takes:
  [app/identification.md](../../app/identification.md#where-the-ciphertext-is).

Both models verified here decrypt this way (2026-09-09/10, 7ba6bbc, 1a7c66d). The tools
that decrypt captured and live payloads:
[development/probes.md](../../development/probes.md#decrypt_probe).

#### Decrypting BLE Advertisements

With the pair's ENC_KEY (key type `0x04`):

1. **Take** bytes 9-24 of the payload.
2. **Decrypt** them: AES-128 ECB, one block, no padding.
3. **Validate** the plaintext by its MAC suffix: [Decryption Validation](#decryption-validation).
4. **Read** the batteries from bytes 1-3: the sending pod, the other pod, the case.

`examples/decrypt_probe.rs` does this for captured payloads. Which keys Cymba
tries: [app/identification.md](../../app/identification.md#what-counts).

#### Decrypted Format

| Byte | Field | Source |
|---|---|---|
| 0 | Connection state, as cleartext byte 8; high bits unexplained | Pro 3, 2026-10-25, #71; both models, 2026-10-28 to 2026-11-10 |
| 1 | Sending pod: bit 7 charging, bits 0-6 the level | Levels: both models, 2026-09-09/10; which pod: Pro 3, 2026-10-25, #71 |
| 2 | Other pod: bit 7 charging, bits 0-6 the level | Levels: both models, 2026-09-09/10; which pod: Pro 3, 2026-10-25, #71 |
| 3 | Case: bit 7 charging, bits 0-6 the level | Both models, 2026-09-09/10 |
| 4-6 | The last three bytes of a host's address, in the order `bluetoothctl` prints it: the host connected, kept after it disconnected until another host connected, but for one change after a disconnect, below | Pro 3, 2026-10-25, #71; both models, 2026-10-28 to 2026-11-10 |
| 7-9 | The last three bytes of the pair's real MAC, or `00 00 00` unless both pods are in the case | Both models, 2026-09-09/10; Pro 3, #71 |
| 10 | Unknown; the values below | Both models, 2026-09-09/10 and 2026-10-28 to 2026-11-10 |
| 11 | Unknown; bits 0-1, bits 2-3 and bit 4 change apart: below | Pro 3, 2026-10-25, #71; both models, 2026-10-28 to 2026-11-10 |
| 12-15 | Unknown; different in every distinct payload | Pro 3, 2026-10-25, #71; both models, 2026-10-28 to 2026-11-10 |

The bytes below are from the app logs of 2026-10-28 to 2026-11-10 ([Runs](#runs)) unless
a sample says otherwise.

- **Byte 0** equaled cleartext byte 8 in all 111 payloads of the second #71 capture:
  `00`, `04` and `08`. The September samples carry `0x10` on top on the Pro 3, and the
  first #71 capture a `0x40`. In the app logs, its low nibble equaled byte 8 in every
  advertisement, on both models.
  - **`0x10` on top** came from the Pro 3 only. It was in every advertisement of the
    PR #137 check's Pro 3 part and of the #64 check, and in none of #71 capture 4 or
    #142's two sessions. In the #62 rename check it was there until both pods were in
    the case (16:45:46), and not after. On 2026-11-10 each pod set it from 0.1-0.3 s
    after its first advertisement from the case (the left pod at 18:51:29, the right at
    18:56:24) to the end of the run, in the case and out of it, but for one
    advertisement of the left pod (19:03:35.126).
  - **`0x40` on top** (`0x44`) came on both models, in a few advertisements at a time,
    each with byte 8 `04` and bytes 4-6 naming the iPhone: #71 capture 4 and
    #142 session 1 (Pro 3), the PR #137 check and 2026-11-02 (Pro 2 (USB-C)).
- **Bytes 4-6** carried the last three bytes of this host's adapter's address in the
  order `bluetoothctl` prints it, as bytes 7-9 carry the pair's MAC. They changed when this host connected, and changed back 1.4 s after it
  disconnected (first #71 capture). In the app logs they named the host connected, and
  kept naming it after it disconnected, until another host connected:
  - 2026-11-10: another host from the run's first advertisement until this host
    connected (18:48:10); this host from then to the end, also while the link was down
    with both pods in the case (19:02:28-19:03:37).
  - #71 capture 4: this host, also while the link was down (19:40:38-19:42:08); the
    iPhone from the advertisement after it took the pods and the link here was reset
    (19:43:43.464); this host from its reconnection (19:43:46.931); the iPhone again
    from 19:44:02.269, 49 ms before the link here was reset, to the end, through the
    pods' stay in the closed case.
  - The #62 rename check: another host from the link reset here at 16:43:29 until this
    host connected at 16:46:26, also while byte 8 was `00` (16:45:52-16:46:26).
- **Byte 10** held:

  | Value | When |
  |---|---|
  | `00` | Byte 8 `00`, every time. This host connected: from 6-9 s after BlueZ reported the connection, in 13 of 15 connections; in the other two, no advertisement came after the last `0f`, at 7 s, until the first `00`, at 13 s and 31 s. Another host connected, not the iPhone: as well (the #62 rename check). The iPhone named, byte 8 `04`: once, between hosts (the #64 check, 16:35:00.227) |
  | `0f` | From the advertisement in which byte 8 went to `04` as a host connected, or in which bytes 4-6 changed to a host that had just connected: until the `00` above, or for 0.3-1.5 s until `06` or `01` with the iPhone |
  | `06` | Pro 3, the iPhone connected, after its `0f`, but for the two stretches below (#71 capture 4; #142 session 1) |
  | `02` | Pro 3, the iPhone connected: 19:46:01-19:46:21, as both pods went into the case and the lid closed (#71 capture 4) |
  | `01` | Pro 3, the iPhone connected: 19:47:06-19:47:26, after it connected again on the lid's opening, then `06` (#71 capture 4). Pro 2 (USB-C): every advertisement naming the iPhone after its `0f`, byte 8 `04` (the PR #137 check; the role switch run, 2026-11-02) |
  | `f0`, `ff`, `f1` | The four advertisements with byte 8 `0c`, and two of the three with `08` ([Byte 8](#byte-8-connection-state)) |

- **Byte 11** held:

  | Bits | What they did |
  |---|---|
  | 0-1 | 1, 2 and 3 about 6 s, 31 s and 62 s after byte 8 went from `00` to `04` (#71 capture 4; the PR #137 check). 1, 2 and 3 about 5 s, 30 s and 60 s after this host disconnected from the worn pods (the PR #137 check). Back to 0 at each connection and disconnection of this host (the PR #137 check), and at byte 8's first `05` (#71 capture 4, twice, the iPhone connected), and 0 while byte 8 stayed `05`. On 2026-11-10: 1 at 6.8 s after byte 8 went to `04`, 0 again 1.1 s later in the first advertisement naming this host, and 0 from then to the end of the run, byte 8 `05` from 18:48:15 |
  | 2-3 | 0 from the first advertisement with both pods in the case, every time (2026-11-10; #71 capture 4; #142 session 2; the #62 rename check; the PR #137 check). 1, 2 and 3 about 5 s, 30 s and 60 s after the first pod was taken out of the case with both in it (2026-11-10, 19:04:48; #71 capture 4, 19:41:45; the PR #137 check, 23:48:15), then unchanged. One pod going into the case left them as they were |
  | 4 | Set in every advertisement naming the iPhone but two, with byte 8 `00` (#142 session 1, 17:00:08); and in the PR #137 check's advertisements naming this host after it disconnected from the worn pods, byte 8 `00`. Clear in every other advertisement, all of 2026-11-10 among them |
  | 5-7 | 0 in every advertisement |

- **Bytes 12-15** differed between every two distinct advertisements: 771 from the Pro 3
  and 100 from the Pro 2 (USB-C). Two payloads equal in every other byte, from the same
  address 53 ms apart, differed there (2026-11-10, 18:48:04.149 and 18:48:04.202).
- **A level above 100** means the part is unavailable: with both pods worn, the
  decrypted case byte was `0xff` beside the cleartext case nibble `0xF` (Pro 3,
  2026-09-11, 17f6591).
- The samples behind this table: [Runs](#runs).

### Decryption Validation

AES always "succeeds": a wrong key gives garbage, not an error. So the plaintext must
be checked before it is trusted, and the check is its MAC suffix.

**The check.** Bytes 7-9 of the plaintext carry the last three bytes of the device's
real MAC. Compare them with the MAC the key is stored under:

```
valid = (plaintext bytes 7-9 == the MAC's last three bytes) or (plaintext bytes 7-9 == 00 00 00)
```

This holds on both models tested (2026-09-10, 1a7c66d). It also checks something
useful: the payload carries its own device's address, so a match says the key it was
decrypted with is that device's.

**Out of the case, the suffix is zeroed.** Unless both pods are in the case (status
bit 2, which has an exception that is [open](#open-questions)), the pods send `00 00 00` in bytes 7-9, whether a host is connected or not. The
suffix went to zero with the first advertisement from out of the case, and came back
about 0.1 s after bit 2 when the pods went back in (Pro 3, 2026-10-25, #71). It was
first seen on a Pro 3 connected and out of the case, the rest of the plaintext intact
(2026-09-11, 17f6591); the sample is under [Runs](#runs). What rejecting that form would
do in Cymba: [app/identification.md](../../app/identification.md#the-suffix).

**Accepting both forms is safe.** The suffix only tests that the key was right, and
zeros test that just as well: a wrong key yields pseudorandom bytes either way. The
union of the two rules costs exactly one bit: a wrong key passes one time in 2^23
rather than 2^24. That was measured at 4 and 4 false accepts over 40M random keys
against one real ciphertext (17f6591, 2026-09-11).

**With the address resolved first** ([addresses.md](addresses.md#resolving-the-advertising-address)),
this is the only check of the plaintext needed.

⚠️ **Do not validate with magic bytes.** Earlier reverse engineering described a marker
of byte 0's upper nibble clear and byte 4 == `0x2D`. Neither model tested here produces
it (2026-09-10, 1a7c66d):

- byte 4 is part of a host's address, whatever that host's address is;
- byte 0 follows the connection state, with `0x10` on top in the Pro 3 samples.

A magic-byte check therefore rejects *correct* decryptions, which leaves every pair
unidentified. Do not add one beside the suffix check either. What that does in
Cymba: [app/identification.md](../../app/identification.md#the-suffix).

## Accuracy Limitations

### Batteries in the clear and encrypted

| | Cleartext (bytes 4-5) | Encrypted (bytes 9-24) |
|---|---|---|
| Steps | 10%, rounded down (0%, 10%, ..., 100%) | 1% (0-100%), the levels AAP reports (source not recorded) |
| Encoding | A nibble: `0x0`-`0x9` = 0-90%, `0xA`-`0xE` = 100%, `0xF` = unknown | Bit 7 charging, bits 0-6 the level |
| Key needed | None | The ENC_KEY, from AAP ([aap/keys.md](../aap/keys.md)) |
| Says which pair sent it | No | Yes |

- **Rounded down:** the September samples (both models, 2026-09-09/10) advertised 58% as
  50% and 14% as 10% in the clear.
- **`0xA` = 100%:** the three Pro 2 (USB-C) September payloads in
  `examples/decrypt_probe.rs` carry byte 4 = `0xaa`, and the two whose decryption is
  recorded ([Runs](#september-samples)) read 100% / 100% (2026-09-10, 1a7c66d).
- **`0xB`-`0xE` = 100%:** from LinuxPods' first notes, after LibrePods at the time
  (3e5033a, 2025-10-12); never recorded here. LibrePods' Linux parser treats only
  `0xF` as special (`linux/ble/blemanager.cpp` at commit 89356c0).
- **The encrypted levels** tracked the cleartext ones on both models (2026-09-09/10,
  7ba6bbc).

Which levels Cymba shows: [app/identification.md](../../app/identification.md#batteries-in-the-clear-and-encrypted).

### Update Frequency

| What | Observed | Source |
|---|---|---|
| Advertisements | About 5 a second from each pod that advertises, in every steady state: open case, out of the case, connected or not | Pro 3, 2026-10-25, #71 |
| Bursts | 30-35 a second for about 10 s after the lid opens or the pods go back in the case | Pro 3, 2026-10-25, #71 |
| Payload | Repeated for hundreds of advertisements; changes in bursts when something happens, again about 5 s, 30 s and 60 s later, then stays | Pro 3 and Pro 2 (USB-C), 987ffe3 |
| Longest unchanged stretch | 163 s, pods worn, connected to this host or to nothing, on both models | Pro 3 and Pro 2 (USB-C), 987ffe3 |
| Idle pair | Same payload for minutes: over 40 s of scanning and across 2 minutes before it, in the open case, not connected to this host | Pro 3, 2026-11-02, #153 |
| Closed lid | The pods stop advertising about 4 s after the lid closes | Pro 3, 2026-10-25 and 2026-11-01, #71 |
| Discovery, or a monitor | When bluetoothd reports the pods' advertisements: [linux/bluez.md](../linux/bluez.md#discovery), and through an advertisement monitor [linux/bluez.md](../linux/bluez.md#discovery-and-directed-advertising) | |

Decrypted byte 11's bits 0-1 and 2-3 stepped at about these times, 5 s, 30 s and 60 s
after an event ([Decrypted Format](#decrypted-format)). On 2026-11-02, in each app run, each pod's payload
also changed once about 2 s after discovery started, then stayed (Pro 3, #153).

**Which pod advertises:**

- While either pod is in the case, both advertise, each from its own address (Pro 3,
  2026-10-25, #71).
- With both out of the case, worn or not, only the primary pod advertises: the right
  one in the first two #71 captures, where it was primary (Pro 3, 2026-10-25).
- In session 1 of #142, with both pods worn and the left primary at times, the
  advertisement's primary agreed with the primary pod AAP reported 13 times out of 13
  (Pro 3, 2026-11-01).

## Comparison: BLE vs AAP

### BLE Proximity Pairing (This Protocol)

| Feature | Cleartext | Encrypted |
|---|---|---|
| Battery steps | 10% | 1% |
| Identifies the pair | No | Yes (IRK, then ENC_KEY) |
| Update rate | On events | On events |
| Connection required | No | No, but the keys come over AAP |
| Works with an iPhone connected | Yes | Yes |
| Control (noise modes) | No | No |
| Keys required | No | IRK and ENC_KEY, from AAP |

What Cymba shows of each: [app/identification.md](../../app/identification.md#batteries-in-the-clear-and-encrypted).

### AAP (Apple Accessory Protocol)

| Feature | AAP |
|---|---|
| Battery | 1%, live |
| Update rate | Under 1 s (source not recorded) |
| Connection required | Yes, L2CAP PSM 4097 |
| Works with an iPhone connected | No: connecting the pods here takes them from the iPhone ([linux/bluez.md](../linux/bluez.md#a-pair-another-host-holds)) |
| Control commands (noise modes, say) | Yes |

**Which to use:**

- **AAP** for live battery and control, while the AirPods are connected to this host.
- **BLE, encrypted,** for passive monitoring while they are connected elsewhere. The
  keys come once, over AAP.
- **BLE, cleartext,** cannot tell one pair from another behind their random addresses,
  so it is no basis for monitoring a pair. It can serve to offer pairing to AirPods
  nearby that no key identifies (#131).

The addresses the advertisements come from, and how the IRK resolves them:
[addresses.md](addresses.md).

## Runs

### September samples

Advertisements captured from the running app on 2026-09-09/10 from a Pro 3 and a
Pro 2 (USB-C), decrypted with each pair's key (7ba6bbc, 1a7c66d;
`cargo run --example decrypt_probe` decrypts the captured payloads). The real MACs
and the host address are placeholders:

```
Pro 3 (0x2027), real MAC AA:BB:CC:DD:EE:FF
10 be be 9f a1 a2 a3 [DD EE FF] 00 00 a7 8a b5 cc   -> 62% / 62% / 31%
10 c8 ca ba a1 a2 a3 [DD EE FF] 00 00 7e 5b ee 7a   -> 72% / 74% / 58%

Pro 2 (USB-C) (0x2024), real MAC 11:22:33:44:55:66
00 e4 e4 8e a1 a2 a3 [44 55 66] 00 00 3b a2 15 bd   -> 100% / 100% / 14%
00 e4 e4 8e a1 a2 a3 [44 55 66] 00 00 03 f4 13 e0   -> 100% / 100% / 14%
   ^^ ^^ ^^ ^^ ^^ ^^ ^^^^^^^^^^ ^^^^^
   |        |        |          bytes 10-11
   |        |        MAC suffix
   |        a host
   batteries
```

- Within one device, only bytes 1-3 (batteries) and 12-15 change between these samples.
- Bytes 4-6 are the same for both models: both named the same host.
- The suffix matched the real MAC on every sample with both pods in the case.
- The `0x2D` marker at byte 4 was not seen on either model.
- The cleartext `0x51`/`0x59` lid vectors of that month, from the Pro 3, all have the
  case charging (#71).

A Pro 3 connected and out of the case, 2026-09-11 (17f6591), the rest of the plaintext
intact:

```
14 50 4d ff a1 a2 a3 [00 00 00] 00 0f d4 65 7c 03   -> 80% / 77% / case unknown
   ^^ ^^ ^^ ^^ ^^ ^^ ^^^^^^^^^^ ^^^^^
   |        |        |          bytes 10-11
   |        |        zeroed suffix
   |        a host
   batteries
```

Its cleartext bytes 3-5 are `0b 78 8f`: the right pod sending (status bit 5 clear), both
pods worn, byte 4 = `0x78`, and the case nibble `0xF` (`examples/decrypt_probe.rs`).

The firmware recorded with the earliest runs here, 2025-10: 7A305 on the Pro 2 (USB-C)
(3e5033a), 8A353 on the Pro 3 (24ef39a).

### The #71 captures

AirPods Pro 3 on one adapter (#71):

| Capture | Date | Tools |
|---|---|---|
| 1 and 2 | 2026-10-25 | `btmon`, `dbus-monitor --system "sender='org.bluez'"` and the app at `RUST_LOG=linuxpods=trace`, build 06a426d |
| 4 | 2026-11-01 | The app at `RUST_LOG=linuxpods=trace`, build df29b14 |

- **Status byte, one pod in the case:** with both pods in the case, only one pod at a
  time set bit 6: the right pod's `0x15` beside the left's `0x75`, and later the left
  pod's `0x35` beside the right's `0x55`.
- **Status byte, bit 4:** the right pod's first advertisement after the lid opened was
  `0x04`, with both pods in the case and bit 4 clear. Being put back, the left pod sent
  `0x29`, `0x22` and `0x20` from the hand while the right pod already sent `0x51` from
  the case.
- **Byte 6 in the open case (capture 2):** the right pod sent `0x11` with the left
  pod's `0x31`, before both sent `0x32` after being put back. This does not fit the
  high-nibble table.
- **Byte 6 on the charger (capture 4):** both pods in the open case went from `0x32` to
  `0x52` half a second ahead of byte 5's charging bit (bit 6), and sent `0x5a` with the
  lid closed. The September `0x51`/`0x59` vectors, all with the case charging, fit.
- **Lid closed (capture 2):** both pods sent byte 6 = `0x39` within 22 ms of the marker
  and kept advertising for 3.9 s, then stopped.
- **Capture 4** (19:38-19:48 UTC) went through role switches, one pod into the case and
  the lid closed and opened on it, both pods into the closed case, the iPhone taking the
  pods (19:43:43, and again from 19:44:02 to the end), and the case put on the charger
  (19:46:12) ([aap/ear.md](../aap/ear.md#71-capture-4-pro-3-2026-11-01)).

### The app logs of 2026-10-28 to 2026-11-02

The app's own logs of other checks, each with every advertisement whole; AAP packets
whole only where the table says so. No firmware version was logged. Their
advertisements were decrypted here with each pair's ENC_KEY, as in
[Decrypting BLE Advertisements](#decrypting-ble-advertisements). Times are UTC.

| Run | Date, UTC | Model | Build, log | What the pods did |
|---|---|---|---|---|
| #136 | 2026-10-28, 20:21-20:25 | Pro 3 | a791646, `linuxpods=trace` | Worn |
| The PR #137 check | 2026-10-28 23:47 to 2026-10-29 00:04 | Pro 3, then Pro 2 (USB-C) | 8bc90ba, `linuxpods=trace`, AAP packets cut to 8 bytes | Pro 3: into the case, the lid closed for 17 s and opened, out; this host connected and disconnected 14 times in 6 minutes, the pods worn; back into the case. Pro 2 (USB-C): the iPhone and this host taking it in turn |
| The #64 check | 2026-11-01, 16:34-16:36 | Pro 3 | df17831, `linuxpods=debug`, AAP packets cut to 8 bytes | Out of the case; another host, the iPhone, then this host connecting |
| #142 session 1 | 2026-11-01, 16:57-17:16 | Pro 3 | df17831, `linuxpods=debug`, AAP packets cut to 8 bytes | Out of the ears and back; the iPhone took the pods twice ([aap/ear.md](../aap/ear.md#session-1-pro-3-2026-11-01)) |
| The #62 rename check | 2026-11-01, 16:43-16:49 | Pro 3 | df17831, `linuxpods=debug`, AAP packets cut to 8 bytes | Another host took the pods at 16:43:29; both into the case, the lid closed for 5 s (16:45:47-16:45:52), out; this host connected at 16:46:26 |
| #142 session 2 | 2026-11-01, 17:21-17:25 | Pro 3 | a local build of df17831, `linuxpods=debug`, AAP packets up to 64 bytes | Role switches, each pod into the case with the lid open and closed, both into the case and out ([aap/ear.md](../aap/ear.md#session-2-pro-3-2026-11-01)) |
| #62 first pairing | 2026-11-01, 17:36-17:39 | Pro 2 (USB-C) | 8d6938e, `linuxpods=debug`, AAP packets cut to 8 bytes | Both in the case, the case at 1%; the lid opened, paired again, out |
| Role switch | 2026-11-02, 19:25-19:29 | Pro 2 (USB-C) | d289626, `linuxpods=trace` | The iPhone held the pods until this host connected at 19:26:25; worn, the primary out and back eight times ([aap/ear.md](../aap/ear.md#session-3-pro-2-usb-c-2026-11-02)) |

#71 capture 4 and the 2026-11-10 run are logs of the same kind: "the app logs of
2026-10-28 to 2026-11-10" are these eight and those two.

### The #184 run

AirPods Pro 3, 2026-11-07: the app on main (296f726) listening through an
advertisement monitor (Intel AX211, BlueZ 5.87), at `RUST_LOG=linuxpods=trace`, its
`BLE parsable raw:` line cut to bytes 0-8. Both pods were worn and connected over AAP,
which read the left pod at 56-57% and the right at 80% throughout; the left had no AAP
level in the closed case. The left pod was taken out of the ear for a moment, then put
in the case with the lid closed for about 10 s, which made the right pod primary and
advertise, then taken out again.

| Bytes 0-8 | Sender (bit 5) | Low nibble | High nibble |
|---|---|---|---|
| `01 27 20 2b 85 8f 11 00 05` | Left | `5`, left | `8`, right |
| `01 27 20 29 85 8f 11 00 05`, the left pod out of the ear | Left | `5` | `8` |
| `01 27 20 71 85 98 5a 00 04`, `01 27 20 73 85 98 5a 00 05`, the left pod in the case | Left | `5` | `8` |
| `01 27 20 13 58 a8 11 00 05` | Right | `8`, right | `5`, left |
| `01 27 20 03 58 8f 11 00 04`, the left pod out of the ear (AAP's ear report) | Right | `8` | `5` |
| `01 27 20 02 f8 8f 11 00 04`, the left pod in the closed case (AAP's ear report) | Right | `8` | `f`, unknown |
| `01 27 20 24 f5 98 58 00 00`, `01 27 20 24 f5 98 51 00 00`, the left pod in the case around the lid opening | Left | `5` | `f`, the right pod worn at 80% |

- Each pod sent its own level in the low nibble, and the other pod's in the high one.
- The `f` in `24 f5` is the right pod's level, unknown in the left pod's first
  advertisements around the lid opening, as in the 2026-11-10 run's `24 fa`
  ([The other pod's fields](#the-other-pods-fields)).

### The 2026-11-10 run

AirPods Pro 3, firmware 9A348, 2026-11-10: a plain build of main (ec2b13f) listening
through an advertisement monitor (BlueZ 5.87, one Intel USB adapter, `btusb`
8087:0aaa), at `RUST_LOG=cymba=trace`. The pair was connected over AAP throughout,
except while both pods were in the closed case. The run went through 15 steps, each
held 65 s, past the payload changes that follow an event
([Payload changes](#payload-changes)). Most steps made one change; two made more: one put
the left pod back in the ear and then the right pod in the open case, another took both
pods out and then closed the empty case. Where each pod was comes from AAP's ear report
(`0x06`) and primary pod packet (`0x08`) ([aap/ear.md](../aap/ear.md)).

The status byte in each state, after the pods had settled:

| Where the pods were | Status, by sender | Bit 4 |
|---|---|---|
| Both worn, the case closed, then open and empty | Right `0b`, the payload unchanged for 2.5 min, about 70 s of it with the case open | Clear |
| Both on the desk, the empty case closed, then open | Right `01` | Clear |
| Right worn, left out of the ear and out of the open, empty case | Right `03` | Clear |
| Left in the open case, right worn | Left `73`, right `13` | Set |
| Right in the open case, left worn | Left `33`, right `53` | Set |
| Right in the open case, left in the hand | Left `31`, right `51` | Set |
| Both in the open case | Left `35`, right `55` | Set |
| Both in the case, the lid opened after about 70 s closed, the link down | Left `34`, right `14` | Set |
| The same, once AAP was back and its ear report put both in the open case | Left `75`, right `15` | Set |
| Left in the closed case, right worn | Right `02` | Clear |
| Right in the closed case, left worn | Left `22` | Clear |

A pod in the case whose partner was out sent a few advertisements first that fit none
of these rows. Bytes 3-8:

| Bytes 3-8 | Sender | Where the pods were | What came next |
|---|---|---|---|
| `24 fa 95 11 00 00` | Left | The left pod in the case, the lid just opened; the right pod worn | AAP's `0x08` and `0x06` 12 s later, then left `73`, right `13` |
| `04 fa 95 11 00 00` | Right | The right pod in the case, the lid just opened; the left pod worn | AAP's `0x08` and `0x06` 10 s later, then left `33`, right `53` |
| `0d aa 95 31 00 05` | Right | The right pod in the open case for 0.8 s, per AAP's ear report; the left pod worn | AAP's `0x08` and `0x06` 0.1 s later, then right `51`, left `33`, right `53` |
| `05 9a b5 11 00 05` | Right | The right pod in the open case; the left pod out of it for 0.2 s, per AAP's ear report | AAP's ear report put both out 45 ms later; then right `01` |

- Status `0x24`, byte 8 `00` and the `f` in byte 4's high nibble are as in #184's
  `24 f5` ([The #184 run](#the-184-run)); byte 6 is `0x11`, where #184 had `0x58` and
  `0x51`. The right pod's `04 fa` has the `f` too.
- Closing the lid kept bit 4 in the last advertisements before the pods fell silent:
  the left pod's `73` with byte 6 `0x39`, alone in the case; with both in the case,
  `55` and `35` with `0x39` and `0x3a`.
- With both pods in the case, at most one set bit 6: `55` beside `35`, `75` beside
  `15`, and neither in `34` and `14`.
- On its way to the case, with the left pod worn, the right pod was out of the ear for
  2.4 s, back in it (`0b`), then out again for 0.3 s, and sent `09` each time it was
  out, bit 4 clear.
- Just after the lid opened on its partner, byte 4's high nibble and byte 5's low
  nibble were `f` from the worn pod: the right pod's `13 fa af` at 18:54:00.801, then
  `13 aa a5` 0.3 s later; the left pod's `33 fa af` at 18:58:54.351, then `33 aa a5`
  0.5 s later. With the left pod just put in the open case, the worn right pod sent
  `13 aa af` at 18:51:29, unchanged until the lid closed; with the right pod just put
  in, the worn left pod sent `33 aa a5` at 18:56:25. The first advertisements of the
  run, at 18:47:59 before the AAP link, were the right pod's `00 fa` and `02 fa`, then
  `03 aa`, all with `8f`.
- Byte 5 was `b5` in every advertisement with both pods in the case, `95` from a pod
  alone in the case, `a5` or `af` from its partner, and `8f` with both out, but for the
  right pod's `05 9a b5` ([Byte 5](#byte-5-charging-status)).
- As the left pod went from the hand back into the case beside the right pod, it sent
  `33` and `31`, then `35`: bit 4 set throughout.

### Payload changes

- 987ffe3: with the pods worn, connected to this host or to nothing, the payload
  changed again about 5 s, 30 s and 60 s after an event and then stayed; 163 s in the
  longest stretch, on a Pro 3 and a Pro 2 (USB-C). The dates of these runs are not
  recorded.
- #153, 2026-11-02: a Pro 3 idle in the open case, a 40 s LE scan
  (`bluetoothctl scan le`, D-Bus signals recorded). Each pod sent about 178
  `RSSI`-only `PropertiesChanged` and no `ManufacturerData` change. The payloads were
  the bytes the app had received 2 minutes earlier.

## References

- [LibrePods](https://github.com/librepods-org/librepods) - an open source Android and
  Linux AirPods client. Its Linux parser is `linux/ble/blemanager.cpp` and
  `linux/ble/blemanager.h`.
- [AirPodsDesktop](https://github.com/SpriteOvO/AirPodsDesktop) - its product IDs are in
  `Source/Core/AppleCP.cpp`.
- [OpenPods](https://github.com/adolfintel/OpenPods) - an open source Android AirPods
  client.
- [furiousMAC/continuity](https://github.com/furiousMAC/continuity) - Apple Continuity
  protocol documentation and a Wireshark dissector.
- BlueZ D-Bus API documentation.

## Open questions

- Status bit 0: what it means. With an AAP link up it matched `0x08`'s `SS` but twice;
  what it follows with no link is open ([Byte 3](#byte-3-status-byte)).
- Status bit 7: what it means, and whether any state sets it; it was clear in every
  advertisement here.
- Status bit 6: why the left pod sent `0x69`, bit 6 set, from out of the case (#71
  capture 4, 19:39:57.685).
- Byte 5 bit 7: what it means; it was set in every byte 5 captured here, on both models
  ([Byte 5](#byte-5-charging-status)).
- Byte 6 bits 0-2: what they count. They are not a lid-open counter; what they did is
  under [Byte 6](#byte-6-lid-state).
- Byte 6's high nibble: why a pod in the open case sent `0x1` after the lid opened, for
  over two minutes in some runs and not at all in others, as the right pod's `0x11` in
  capture 2 of #71 did; why `0x5` came beside a case byte 5 called not charging (#142
  session 2); and what the Pro 2 (USB-C)'s `0x7` is ([Byte 6](#byte-6-lid-state)).
- Byte 8: what `0x05` says, which stood while this host's link was up, the pods in the
  case or out of it (2026-11-10); and what `0x08` and `0x0c` mean. Both came only in single advertisements, or two
  in a row, as a connection was set up or the pods went from one host to another
  ([Byte 8](#byte-8-connection-state)).
- Decrypted byte 0: what its high bits (`0x10`, `0x40`) mean, and what sets and clears
  `0x10`. Do not validate against it.
- Decrypted bytes 4-6: whether anything but a host connecting changes them. In capture 1
  of #71 they changed back 1.4 s after this host disconnected; which host they named
  then was not recorded.
- Decrypted byte 10: what its values mean; `06`, `02` and `01` came only with the
  iPhone connected.
- Decrypted byte 11: what bits 0-1, bits 2-3 and bit 4 count or say.
- Decrypted bytes 12-15: what they are.
- The payload length: every payload seen is 25 bytes. Whether a payload of another
  length keeps the encrypted bytes at byte 9 or at the end is unknown.
- Cleartext nibbles `0xB`-`0xE`: never recorded here.
- Status bit 4, two kinds of advertisement that do not fit the rule (Pro 3):
  - Being put back, the left pod sent `0x29`, `0x22` and `0x20` from the hand, bit 4
    clear, while the right pod sent `0x51` from the open case (capture 2 of #71).
    `0x29` also sets bit 3, the right pod's in-ear bit. In the 2026-11-10 run, the left
    pod in the hand beside the right pod in the open case sent `0x31`, bit 4 set, for
    over a minute, and `0x33`, `0x31` and `0x35` as it went back into the case
    ([Runs](#the-2026-11-10-run)).
  - In the #71 captures, the right pod's first advertisement after the lid opened, with
    both pods in the case, was `0x04`, bit 4 clear. In the 2026-11-10 run, the first
    advertisements after the lid opened on both pods in the case were `0x34` and
    `0x14`, bit 4 set.
- Byte 4 on the Pro 2 (USB-C): which nibble carries the sender's level. Every sample
  has `0xaa`, which cannot tell the halves apart; the check:
  [app/identification.md](../../app/identification.md#hardware-checks).
- Status bit 2 set by a pod in the case while its partner is out, with bits 4 and 6
  clear: the left pod's `0x24`, in the case around the lid opening, the right pod worn
  (Pro 3, 2026-11-07, #184); and `0x24`, `0x04`, `0x0d` and `0x05`, each among a pod's first
  advertisements after it went into the case, after the lid opened on it, or as its
  partner left the case (Pro 3, 2026-11-10; [Runs](#the-2026-11-10-run)). What bit 2
  means there is open.
- Why each pod's payload changed once about 2 s after discovery started (Pro 3,
  2026-11-02, #153). Under an advertisement monitor, with no active scan, the payloads
  changed within 0.5 s of each pod being first heard, one of them twice; under discovery
  right after, they did not
  ([linux/bluez.md](../linux/bluez.md#runs), runs C and C2, #189).
- Models other than the Pro 3 and the Pro 2 (USB-C): untested. The MAC-suffix rule and
  the byte-reversed IRK hold on both; older hardware is unknown.
  `examples/irk_probe.rs` checks a new model's addresses
  ([addresses.md](addresses.md#open-questions)).
- Color values other than `0x00` (White): unverified.

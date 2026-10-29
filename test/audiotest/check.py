#!/usr/bin/env python3
"""check.py CAPTURE MARKS: judge the emulator's capture of an audio sweep.

CAPTURE is the card's capture, in either of two forms (told apart by the first
four bytes):

  Octane  a WAV, 44.1 kHz stereo 16-bit (IRIS_OCTANE_AUDIO_WAV). MARKS holds
          byte offsets into its data chunk.
  O2      a dump of what MACE audio played at the guest's own rate
          (IRIS_O2_AUDIO_DUMP): records of out:u8, rate:u32, pairs:u32, then
          pairs x (l:i16, r:i16), little-endian. MARKS holds byte offsets into
          the file. A case is the records that start inside its span; they
          are joined per (output, rate) and the group with the most samples
          is measured at its own rate. A guest that changes rate after it
          starts writing leaves a few records at the old rate; they are not
          part of the tone.

MARKS has one line per case, written by run.sh: "NAME START END", the offsets
before and after that case played. Each slice must hold one tone, so a case
passes when

    its length is within 5% of 2.0 s    (a wrong playback rate scales it), and
    its pitch is within 3% of 1000 Hz   (the same error scales the pitch), and
    it is not silent.

Length is the span of samples above 1/8 of the slice's peak; pitch is the
left channel's zero crossings over that span. Prints one row per case and
exits 1 if any case fails. Standard library only.
"""
import array
import struct
import sys

WAV_RATE = 44100
EXPECT_S, LEN_TOL = 2.0, 0.05
EXPECT_HZ, HZ_TOL = 1000.0, 0.03


def measure(raw, rate=WAV_RATE):
    """(seconds, hz, peak) of the tone in a 16-bit stereo slice at RATE."""
    raw = raw[: len(raw) // 4 * 4]
    s = array.array("h", raw)
    if sys.byteorder == "big":
        s.byteswap()
    left = s[0::2]
    peak = max((abs(x) for x in left), default=0)
    if peak == 0:
        return 0.0, 0.0, 0
    loud = [i for i, x in enumerate(left) if abs(x) > peak / 8]
    seg = left[loud[0] : loud[-1] + 1]
    crossings = sum(1 for i in range(1, len(seg)) if (seg[i - 1] < 0) != (seg[i] < 0))
    secs = len(seg) / rate
    return secs, crossings / 2 / secs, peak


def dump_case(data, a, b):
    """(raw samples, rate) for the dump records starting in [a, b)."""
    outs = {}  # (output, rate) -> [sample bytes]
    pos = 0
    while pos + 9 <= len(data):
        out, rate, pairs = struct.unpack_from("<BII", data, pos)
        body = data[pos + 9 : pos + 9 + pairs * 4]
        if a <= pos < b:
            outs.setdefault((out, rate), []).append(body)
        pos += 9 + pairs * 4
    if not outs:
        return b"", WAV_RATE
    (_, rate), parts = max(outs.items(), key=lambda kv: sum(map(len, kv[1])))
    return b"".join(parts), rate


def main():
    cap, marks = sys.argv[1], sys.argv[2]
    with open(cap, "rb") as f:
        data = f.read()
    is_wav = data[:4] == b"RIFF"
    if is_wav:
        data = data[44:]
    fails = 0
    print(f"{'case':<28} {'length_s':>8} {'pitch_hz':>8}  verdict")
    for line in open(marks):
        name, a, b = line.split()
        if is_wav:
            secs, hz, peak = measure(data[int(a) : int(b)])
        else:
            raw, rate = dump_case(data, int(a), int(b))
            secs, hz, peak = measure(raw, rate)
        ok = (
            peak > 0
            and abs(secs - EXPECT_S) <= EXPECT_S * LEN_TOL
            and abs(hz - EXPECT_HZ) <= EXPECT_HZ * HZ_TOL
        )
        fails += not ok
        print(f"{name:<28} {secs:>8.3f} {hz:>8.0f}  {'ok' if ok else 'FAIL'}")
    print(f"{fails} failed")
    sys.exit(1 if fails else 0)


main()

# IRIX flood ping loses replies that come back too fast

**Symptom:** on an Indy with IRIX 6.5.22, `ping -f -q -c 2000 192.168.0.1`
with the default 56 data bytes reports 80-90% packet loss, at about 100
packets/s. With `-s 1400` it loses 0.3-2%, at about 500 packets/s. The
interface and IP count every reply: `seeq status` rx_delivered matches
`netstat -i` Ipkts, and `netstat -s -p icmp` shows one echo reply input per
request.

**Cause (IRIX):** the replies are dropped between ICMP input and ping's
socket. IRIX's raw-socket input looks up the sockets a packet matches
without the socket lock, then locks each one and checks the match again.
An unconnected raw socket (ping's) is connected to the destination for the
length of each `sendto` and disconnected after it. A reply that arrives
while ping is still inside that `sendto` matches the connected socket in
the unlocked pass and no longer matches once the lock is taken (after the
`sendto` has finished), so it is freed. This reading comes from the
disassembly of the 6.5.22 IP22 kernel and breakpoint hit counts over 300
flood pings: the raw input routine ran 300 times; for ping's socket, 31
replies went on to the append and 269 were freed at the re-check, and ping
reported 31 received. ping -f sends its next request only when it reads a
reply or after 10 ms, hence the ~100 packets/s.

Large replies lose less because they take longer to come back: the guest
spends longer transmitting the request and copying in the reply.

**Why it shows here:** the gateway answers an echo to its own address in
the same pass that reads the request, so the reply can be in the guest's
receive ring within microseconds, while the guest is still returning from
`sendto`. On a 10 Mb/s wire the request and reply alone take about 190 µs
for 56 data bytes, plus the far host's turnaround. Delaying the gateway's
local echo replies by a fixed host time, as an experiment (not committed):

| delay (µs) | loss, 1000 pings of 56 bytes | sent/s |
|---|---|---|
| 0 | 80.4% | 103 |
| 200 | 33.7% | 117 |
| 500 | 1.7% | 451 |
| 1000 | 0.1% | 486 |
| 2000 | 0.1% | 331 |

**Not changed.** The race is IRIX's, and only replies that beat the end of
the sending `sendto` reach it. Holding every reply back by half a
millisecond or more of host time would cost every lock-step exchange (TFTP,
NFS, DNS) that much per packet, and a fixed host delay would still not
track the guest's own speed. Flood ping with small packets is the one
observed casualty; ordinary `ping` loses nothing. Re-measure with the delay
experiment above if a guest workload turns out to depend on it.

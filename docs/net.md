# Networking

Anshi gets online by itself. At boot the kernel brings up every network
card it finds: IPv4 over DHCP, and IPv6 from the router's advertisements
(SLAAC). Each card is its own active interface, with its own addresses
and settings. `net` shows the connections and changes them; `ping`,
`fetch` and `serve` use them.

```
> net                                   # the settings screen
> ping example.com
> ping -6 fec0::2                       # over IPv6
> fetch https://example.com/            # print a page (TLS, certificate checked)
> fetch http://10.0.2.2:8000/f f        # save one (the host, under QEMU)
> serve 80 /                            # serve files to the network
```

## Running it in QEMU

`scripts/run-qemu.sh` attaches a `virtio-net-pci` card on QEMU's
user-mode network: a NAT with DHCP and router advertisements, so there is
nothing to set up on the host and no root needed. Inside, the guest is
`10.0.2.15` and `fec0::<from the MAC>`, the host is reachable as
`10.0.2.2` / `fec0::2`, and `10.0.2.3` forwards DNS. `--pc` mode uses an
Intel `e1000` instead. A `virtio-rng-pci` device feeds the kernel's
random numbers (in `--pc` mode the CPU's RDRAND/RDSEED do).

| Variable | Effect |
|---|---|
| `ANSHI_NET=off` | no network card at all (also suppresses the e1000e q35 would add by itself) |
| `ANSHI_NET=2` | a second card (an e1000) on a second network, `10.0.3.0/24` and `fec1::/64`: two active interfaces |
| `ANSHI_NET_FWD="tcp:127.0.0.1:8080-:80"` | forward host ports into the guest (QEMU `hostfwd` rules, space-separated), for `serve` |
| `ANSHI_NET_PCAP=<file>` | also write every frame on the first network to a pcap file, for Wireshark or `tcpdump -r` |

To serve files to the guest, run `python3 -m http.server 8000 --bind
127.0.0.1` (or `--bind ::1` for IPv6) on the host and `fetch
http://10.0.2.2:8000/<file> <name>` inside. The other way round:
`ANSHI_NET_FWD="tcp:127.0.0.1:8080-:80" ./scripts/run-qemu.sh`, `serve`
inside, then `curl http://127.0.0.1:8080/` on the host.

Whether `ping` reaches the internet depends on the host letting QEMU send
ICMP (macOS does). TCP and UDP work everywhere; the gateway always
answers ping. QEMU's NAT doesn't pass pings *in*, but the kernel answers
any that arrive, on a real network.

## The `net` screen

The interface list on the left, the selected one's details on the right:
state, IPv4 address, gateway and DNS, IPv6 addresses (global first, then
link-local) and router, MAC, its role -- whether traffic to other
networks and name lookups go through it -- how long it has been up, and
traffic. It refreshes twice a second. In a narrow terminal (a `tile`
pane) the list is dropped and the details fill the pane.

| Key | Action |
|---|---|
| `j`/`k`, arrows | select an interface |
| `r` | ask the DHCP server again |
| `s` | set a static IPv4 address: a form for address/prefix, gateway and DNS (`Tab` moves, `Enter` applies, `Esc` cancels) |
| `d` | go back to DHCP |
| `p` | connection test: ping the gateway and the IPv6 router, look up `example.com`, open its port 80 |
| `q`, `Esc` | quit |

Each acts on the selected interface. Changes take effect at once and are
written to that interface's section of `/etc/net.conf`, then flushed to
disk, so they hold across reboots:

```
[eth0]
mode=dhcp
[eth1]
mode=static
address=192.168.1.20/24
gateway=192.168.1.1
dns=192.168.1.1
```

`mode=dhcp`, no section, or no file means DHCP; lines before any
`[ethN]` belong to eth0. The kernel reads the file at boot, after the
filesystem loads. IPv6 has nothing to set: it configures itself.

## `ping`, `fetch` and `serve`

- **`ping [-4|-6] <host> [count]`**: ICMP or ICMPv6 echo, one a second, 4
  by default. `Ctrl-C` or `q` stops it. A link-local address takes its
  interface after a `%`: `ping fe80::2%eth0`. Times are at the kernel
  tick's 10 ms resolution, so a LAN reply shows as `0 ms`. Late replies
  still count. Under QEMU the first ping to a new host often takes about
  a second: smoltcp waits a second between address lookups (ARP, NDP),
  so if one just happened -- the DNS server's, say -- the next waits.
- **`fetch [-4|-6] <url> [file]`**: an HTTP/1.1 `GET` over `http://` or
  `https://`. The body goes to stdout, or into `file`. Follows up to five
  redirects (http to https included), understands chunked replies, holds
  at most 8 MiB. IPv6 hosts go in brackets: `http://[fec0::2]:8000/`.
  `-4`/`-6` pick the IP version a name resolves to; by default IPv4 is
  tried first.
- **`serve [port] [dir]`**: an HTTP/1.0 file server, port 80 and the
  current directory by default, over IPv4 and IPv6 on every interface.
  Files are served as they are, directories as their `index.html` or a
  generated listing, one request at a time (the kernel queues a few more
  meanwhile). It prints the addresses it can be reached at, logs one line
  per request, and refuses paths that climb out of `dir`. `q` or `Ctrl-C`
  stops it.

The kernel shell also has `netinfo`, the same facts as `net` in text.

## HTTPS

`fetch` speaks TLS 1.2 and 1.3 through
[rustls](https://github.com/rustls/rustls), with the
[RustCrypto](https://github.com/RustCrypto) algorithms
(`rustls-rustcrypto`: pure Rust, no C, no assembly, no std). It checks
certificates properly:

- against **Mozilla's root store**, compiled into `fetch`
  (`webpki-roots`) -- a self-signed or unknown-CA certificate is refused;
- for the **host name** asked for;
- at the **current time**, from the PC's real-time clock
  (`drivers/rtc.rs`, read at boot as UTC). An expired certificate is
  refused, and so is every certificate if the clock is badly wrong: a PC
  whose clock Windows keeps in local time is off by the time zone, which
  matters only within hours of a certificate's start or end.

Its randomness comes from the kernel's `getrandom()` (below). There's no
way to skip the checks.

rustls without std offers only its "unbuffered" connection, a state
machine that says what it needs next; `nettools/src/tls.rs` drives it
over a TCP socket. `rustls-rustcrypto` is a pre-release crate: it works
and is what makes a bare-metal build possible, but it hasn't had the
review the mainstream providers have.

## How it works

```
 ping / fetch / serve / net   ring 3: anshi::net (userland/rust/anshi/src/net.rs)
        │  socket, connect, bind, listen, accept, sendto, recvfrom, read, write, close
        │  net_info, net_config, resolve, getrandom
 exec/syscall.rs              OpenFile::Socket { id }
 net/socket.rs                the socket table, refcounted across fork()
 net/mod.rs                   one smoltcp interface per card: ARP/NDP, IPv4, IPv6,
                              ICMP, UDP, TCP, DHCP, SLAAC, DNS; routing between them
 drivers/nic/mod.rs           NicDriver: whole Ethernet frames in and out
 drivers/virtio/net.rs  drivers/nic/e1000.rs
```

- **The stack is [smoltcp](https://github.com/smoltcp-rs/smoltcp)**:
  `no_std`, integer-only (the kernel is soft-float), with DHCP, SLAAC and
  DNS clients along with TCP.
- **Interfaces.** Every card gets its own smoltcp `Interface` and socket
  set, its own DHCP client and DNS socket, and its settings from its own
  section of `/etc/net.conf`. Its IPv6 addresses are a link-local one
  made from the MAC and whatever SLAAC forms from the router's prefix.
- **Routing.** Traffic leaves through an interface whose network holds
  the destination; otherwise through the first (lowest-numbered) one
  with a default route for that IP version. A link-local IPv6
  destination goes out the interface its scope names (`%eth1`), else the
  first linked one. Name lookups go to the first interface with a DNS
  server. `net` and `netinfo` show which interface has which role.
- **Sockets across interfaces.** A connected TCP socket lives on the
  interface its route picked. A listening one keeps a small backlog of
  smoltcp listeners on every interface; `accept()` hands out whichever
  completed a handshake and puts a fresh listener in its place. A bound
  UDP socket is bound on every interface, and receives from all of them.
  Listening and bound sockets take both IPv4 and IPv6, whatever family
  they were opened with.
- **Polled, like every driver.** `net::poll()` moves frames between the
  cards and the stack and runs every protocol timer. It's called from
  kernel context only, never from the tick handler (see `sync.rs`):
  each pass of the scheduler in `process::run_until()`, the keyboard
  wait in `read()` and the kernel shell's `read_line()`, and every
  network syscall. So a frame waits at most one 10 ms tick.
- **Nothing blocks.** A socket call that can't finish yet returns `-11`
  (Linux's `EAGAIN`), and the program loops in ring 3, yielding between
  tries. That's the same rule as pipes, for the same reason: a syscall
  that blocks would stall every other process. `anshi::net` wraps it all
  into blocking calls with timeouts.
- **Connecting by itself.** `net::init()` starts DHCP on every card and
  waits up to 1.5 s for the leases (a real card can need a second just to
  bring its link up), then lets boot carry on; a late lease still lands.
  When a link drops and comes back, DHCP starts over: it may be a
  different network. After boot, status lines go to the serial log only,
  so they never draw over a full-screen program.
- **Sockets outlive nothing.** Every way a process dies (`exit()`, a
  crash) closes its sockets, like its stdio pipes. A closed TCP socket
  finishes its goodbye in the background and is reset if that takes more
  than 3 s.

## Random numbers

`random.rs` is a ChaCha20 generator, seeded at boot and reseeded at most
once a second from every source there is: RDSEED and RDRAND on the CPU,
virtio-rng under QEMU (`drivers/virtio/rng.rs`), and always also the
jitter in how long short timed loops take. Each request ends by replacing
the key with fresh output, so nothing handed out earlier can be
recomputed. The boot log says which sources it found, and warns if timing
jitter was all -- that's unpredictable enough for TCP sequence numbers,
not something to trust TLS keys to. `getrandom()` (Linux's 318) hands it
to programs.

## Drivers

| Card | Where | Notes |
|---|---|---|
| virtio-net (`1af4:1041`) | `drivers/virtio/net.rs` | QEMU's default here. Needs `disable-legacy=on`, or QEMU shows the transitional id (like virtio-blk) |
| Intel 82540EM / 82545EM / 82574L (`8086:100e`/`100f`/`10d3`) | `drivers/nic/e1000.rs` | QEMU `e1000`/`e1000e`, UTM, VirtualBox, VMware, and real Intel boards. Legacy descriptors, polled, multicast on (IPv6 needs it) |
| Realtek RTL8111/8168, RTL8125 (`10ec:8168`/`8161`/`8125`/`3000`) | `drivers/nic/rtl8168.rs` | Most PC boards. **Untested**: QEMU emulates neither, see below |

All register with `drivers/nic`, which names them `eth0`, `eth1`, ...
in the order found. The kernel shell's `nicdebug` dumps each card's
driver state (the Realtek driver's registers and rings, in particular).

### The Realtek driver

QEMU emulates no Realtek gigabit chip, so `rtl8168.rs` is the one
driver here that hasn't run yet: it was written against Linux's `r8169`
and follows its bring-up order step for step -- probe-time setup out of
the firmware's management mode, PHY powered up with autonegotiation
restarted, reset, per-revision tuning, rings, RX/TX on. Each revision has
its own tuning; the boot log names the revision found
(`rtl8168: RTL8168h/8111h (XID 541, VER_46)`). The two a B550 board is
likely to carry get Linux's full sequence: **RTL8111H** (VER_46/48) and
**RTL8125B** (VER_63), and the RTL8125A/D too. Other 8111/8168 revisions
from the 8111b on get the common part only, and say so at boot. Not
ported: Linux's PHY firmware patches, the DASH management engine some
business boards run, and the 8168dp's own PHY access.

Testing it on the PC, with the cable in:

1. The boot log should show `rtl8168: <chip> ...` and `rtl8168: ready,
   mac ..., link up, 1 Gb/s` (or 2.5), then the DHCP lease.
2. `netinfo` in the kernel shell (or `net`), then `ping` the gateway,
   `fetch https://example.com/`.
3. If something fails -- no link, no lease, no replies -- `nicdebug`
   right then, and a photo of its output. `link down` with the cable in
   points at the PHY; a lease that never comes with `rx 0 packets`
   at receive (the RX DV gate, `MISC` bit 19, or `RxConfig`); `tx` slots
   stuck "with the card" at transmit.

## Not yet

- **The Realtek driver running on real hardware** (above).
- **Wi-Fi.** QEMU has no Wi-Fi card to emulate, so this is real-hardware
  work from the start: a USB dongle driver on the xHCI stack (firmware
  upload, raw 802.11 frames), the 802.11 layer (scan, authenticate,
  associate) exposed as one more `NicDriver`, and WPA2-PSK (PBKDF2, the
  EAPOL 4-way handshake, AES-CCMP).
- Static IPv6 addresses, DHCPv6, and DNS servers from router
  advertisements (RDNSS): IPv6 names are looked up through the IPv4 DNS
  server.
- Loopback: a program can't connect to the machine's own address.
- TLS for `serve`, client certificates, and a way to add your own CA.
- Interrupt-driven receive.

# VHS tape for docs/demo.gif, embedded at the top of README.md. Regenerate
# with: vhs docs/demo.tape (needs `vhs`, a system Chrome/Chromium for its
# headless-browser recording step, and outbound ICMP).
#
# Two scripted segments: a healthy live run against 1.1.1.1, then an outage
# demo against 192.0.2.1 (TEST-NET-1, RFC 5737 — should never appear on the
# public Internet, though a local route/VPN/lab network could still answer
# it; the hidden preflight below aborts the recording if it does, so the
# outage state and its growing duration are verified-deterministic, not a
# live-network gamble). The outage segment ends on the
# sustained "no reply" state rather than a Ctrl-C summary: ekg only counts
# an outage in its session summary once it *recovers* (see
# apply_event's PingEvent::Reply branch in src/main.rs), and 192.0.2.1 can
# never recover — so a Ctrl-C here would print "outages: 0" right next to
# a visibly declared outage, which reads as a bug in the recording even
# though it's correct behavior.

Output docs/demo.gif

Require cargo

Set Shell "bash"
Set FontSize 16
Set Width 832
Set Height 528
Set Padding 0
Set Theme "Dracula"
Set TypingSpeed 60ms
Set Framerate 12

# Hidden preamble. Each step prints a sentinel only on success and the
# recording waits on it, so a slow build can't race the demo commands and a
# failed build (or an unexpectedly answering 192.0.2.1) aborts the run at
# the Wait timeout instead of recording a stale binary or a non-outage.
# The sentinels are quoted-split in the Type strings (BUILD_"OK") so the
# Wait regex matches the echoed output, never the typed command itself.
Hide
Type `cargo build --release --quiet && echo BUILD_"OK"`
Enter
Wait+Screen@300s /BUILD_OK/
Type `if case $(uname) in Darwin) ping -c 1 -t 2 192.0.2.1 ;; *) ping -c 1 -w 2 192.0.2.1 ;; esac >/dev/null 2>&1; then echo TARGET_"REPLIES"; else echo PREFLIGHT_"OK"; fi`
Enter
Wait+Screen@30s /PREFLIGHT_OK/
Type "clear"
Enter
Show

Type "./target/release/ekg 1.1.1.1"
Enter
Sleep 7s
Ctrl+C
Sleep 2s

Hide
Type "clear"
Enter
Show

Type "./target/release/ekg 192.0.2.1"
Enter
Sleep 12s

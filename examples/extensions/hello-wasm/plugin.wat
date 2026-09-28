;; hello-wasm: a tack-RPC v3 plugin as a WASI p1 module, hand-written WAT.
;; Speaks JSON-RPC 2.0 over NDJSON stdin/stdout: answers the `initialize`
;; request (id 1, the host's first) with the plugin's capabilities, then
;; loops answering tools/execute / commands/invoke requests (ids parsed
;; from the request line). Notifications and unknown lines are ignored;
;; EOF (shutdown) exits.
(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)

  ;; Layout: 0..8 iovec, 12 nread/nwritten, 16 scratch byte, 20 line len,
  ;; 32..48 digit buffer, 64.. line buffer.

  ;; Read one LF-terminated line into 64.., store its length at 20.
  ;; Returns 1 if any byte was read, 0 on immediate EOF.
  (func $read_line (result i32)
    (local $got i32)
    (local $len i32)
    (loop $loop
      (i32.store (i32.const 0) (i32.const 16))
      (i32.store (i32.const 4) (i32.const 1))
      (drop (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 12)))
      (if (i32.eqz (i32.load (i32.const 12))) (then (return (local.get $got))))
      (local.set $got (i32.const 1))
      (if (i32.eq (i32.load8_u (i32.const 16)) (i32.const 10))
        (then
          (i32.store (i32.const 20) (local.get $len))
          (return (i32.const 1))))
      (i32.store8 (i32.add (i32.const 64) (local.get $len)) (i32.load8_u (i32.const 16)))
      (local.set $len (i32.add (local.get $len) (i32.const 1)))
      (br $loop))
    (i32.store (i32.const 20) (local.get $len))
    (local.get $got))

  ;; Write $len bytes from $ptr to stdout.
  (func $write (param $ptr i32) (param $len i32)
    (i32.store (i32.const 0) (local.get $ptr))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 12))))

  ;; Find $needle in the line buffer; returns its index or -1.
  (func $find (param $needle i32) (param $nlen i32) (result i32)
    (local $i i32)
    (local $j i32)
    (local $limit i32)
    (local.set $limit (i32.sub (i32.load (i32.const 20)) (local.get $nlen)))
    (loop $outer
      (if (i32.gt_s (local.get $i) (local.get $limit)) (then (return (i32.const -1))))
      (local.set $j (i32.const 0))
      (loop $inner
        (if (i32.ge_u (local.get $j) (local.get $nlen))
          (then (return (local.get $i))))
        (if (i32.ne
              (i32.load8_u (i32.add (i32.add (i32.const 64) (local.get $i)) (local.get $j)))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j))))
          (then
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $outer)))
        (local.set $j (i32.add (local.get $j) (i32.const 1)))
        (br $inner))
      (br $outer))
    (i32.const -1))

  ;; Parse the request id: after `"id":`, skip spaces, read digits.
  (func $parse_id (result i32)
    (local $i i32)
    (local $n i32)
    (local.set $i (i32.add (call $find (i32.const 41340) (i32.const 5)) (i32.const 5)))
    (loop $skip
      (if (i32.eq (i32.load8_u (i32.add (i32.const 64) (local.get $i))) (i32.const 32))
        (then (local.set $i (i32.add (local.get $i) (i32.const 1))) (br $skip))))
    (loop $digits
      (if (i32.and
            (i32.ge_u (i32.load8_u (i32.add (i32.const 64) (local.get $i))) (i32.const 48))
            (i32.le_u (i32.load8_u (i32.add (i32.const 64) (local.get $i))) (i32.const 57)))
        (then
          (local.set $n
            (i32.add
              (i32.mul (local.get $n) (i32.const 10))
              (i32.sub (i32.load8_u (i32.add (i32.const 64) (local.get $i))) (i32.const 48))))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br $digits))))
    (local.get $n))

  ;; Write $n in decimal to stdout (digit buffer at 32..48).
  (func $write_u32 (param $n i32)
    (local $pos i32)
    (local.set $pos (i32.const 48))
    (loop $loop
      (local.set $pos (i32.sub (local.get $pos) (i32.const 1)))
      (i32.store8
        (local.get $pos)
        (i32.add (i32.const 48) (i32.rem_u (local.get $n) (i32.const 10))))
      (local.set $n (i32.div_u (local.get $n) (i32.const 10)))
      (br_if $loop (i32.gt_u (local.get $n) (i32.const 0))))
    (call $write (local.get $pos) (i32.sub (i32.const 48) (local.get $pos))))

  (data (i32.const 40000) "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"3.0.0\",\"plugin\":{\"name\":\"hello-wasm\"},\"capabilities\":{\"tools\":[{\"name\":\"ping\",\"description\":\"Answer with a pong from the WASM sandbox\",\"parameters\":{\"type\":\"object\",\"properties\":{}}}],\"commands\":[{\"name\":\"hello-wasm\",\"description\":\"Greet from the WASM sandbox\"}]}}}\0a")
  (data (i32.const 41000) "{\"jsonrpc\":\"2.0\",\"id\":")
  (data (i32.const 41100) ",\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"pong from the WASM sandbox\"}]}}\0a")
  (data (i32.const 41200) ",\"result\":{\"ok\":true,\"note\":\"hello from the WASM sandbox\"}}\0a")
  (data (i32.const 41300) "\"method\":\"")
  (data (i32.const 41320) "tools/execute")
  (data (i32.const 41340) "\"id\":")

  (func (export "_start")
    ;; Handshake: consume the `initialize` request, answer with the
    ;; capabilities result (id 1 — the host's first request).
    (drop (call $read_line))
    (call $write (i32.const 40000) (i32.const 319))
    ;; Main loop: answer requests until EOF (host shutdown closes stdin).
    (loop $main
      (if (call $read_line)
        (then
          (if (i32.ge_s (call $find (i32.const 41300) (i32.const 10)) (i32.const 0))
            (then
              (call $write (i32.const 41000) (i32.const 22))
              (call $write_u32 (call $parse_id))
              (if (i32.ge_s (call $find (i32.const 41320) (i32.const 13)) (i32.const 0))
                (then (call $write (i32.const 41100) (i32.const 77)))
                (else (call $write (i32.const 41200) (i32.const 60))))))
          (br $main))))
)
)

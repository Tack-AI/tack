;; hello-wasm-caps: like hello-wasm, but demonstrates WASM capability
;; grants. The manifest preopens the extension's `data/` directory as the
;; guest's first preopen (fd 3); the `readfile` tool opens `hello.txt`
;; relative to it (path_open) and returns its content. Without the grant
;; the same call fails with an errno (no ambient authority in the sandbox).
;;
;; Protocol: identical NDJSON-over-stdio as every other carrier (see
;; docs/extensions.md). File bytes are sanitized before embedding into the
;; JSON response (demo-grade escaping: " → ', \ → /, controls → space).
(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "path_open" (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_close" (func $fd_close (param i32) (result i32)))
  (memory (export "memory") 1)

  ;; Layout: 0..8 iovec, 12 nread/nwritten, 16 scratch byte, 20 line len,
  ;; 24 opened-fd out, 28 file len, 32..48 digit buffer, 64.. line buffer.
  ;; Data strings at 40000+, file content buffer at 44000..48000.

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

  ;; Open "hello.txt" relative to preopen fd 3 (read-only). Returns the fd,
  ;; or -errno when the sandbox denies the open (e.g. no fs grant).
  (func $open_hello (result i32)
    (local $err i32)
    (local.set $err
      (call $path_open
        (i32.const 3)          ;; first preopened directory
        (i32.const 0)          ;; dirflags
        (i32.const 43000)      ;; "hello.txt"
        (i32.const 9)
        (i32.const 0)          ;; oflags
        (i64.const 2)          ;; fs_rights_base: FD_READ
        (i64.const 0)          ;; fs_rights_inheriting
        (i32.const 0)          ;; fdflags
        (i32.const 24)))       ;; opened-fd out
    (if (i32.ne (local.get $err) (i32.const 0))
      (then (return (i32.sub (i32.const 0) (local.get $err)))))
    (i32.load (i32.const 24)))

  ;; Read up to 4096 bytes of hello.txt into 44000... Returns the length,
  ;; or -errno when the open failed.
  (func $read_hello (result i32)
    (local $fd i32)
    (local $off i32)
    (local.set $fd (call $open_hello))
    (if (i32.lt_s (local.get $fd) (i32.const 0)) (then (return (local.get $fd))))
    (loop $loop
      (if (i32.lt_u (local.get $off) (i32.const 4096))
        (then
          (i32.store (i32.const 0) (i32.add (i32.const 44000) (local.get $off)))
          (i32.store (i32.const 4) (i32.sub (i32.const 4096) (local.get $off)))
          (drop (call $fd_read (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 12)))
          (if (i32.eqz (i32.load (i32.const 12)))
            (then
              (drop (call $fd_close (local.get $fd)))
              (return (local.get $off))))
          (local.set $off (i32.add (local.get $off) (i32.load (i32.const 12))))
          (br $loop))))
    (drop (call $fd_close (local.get $fd)))
    (local.get $off))

  ;; Demo-grade JSON escaping in place: " → ', \ → /, controls → space.
  (func $sanitize (param $len i32)
    (local $i i32)
    (local $b i32)
    (loop $loop
      (if (i32.ge_u (local.get $i) (local.get $len)) (then return))
      (local.set $b (i32.load8_u (i32.add (i32.const 44000) (local.get $i))))
      (if (i32.eq (local.get $b) (i32.const 34))
        (then (i32.store8 (i32.add (i32.const 44000) (local.get $i)) (i32.const 39))))
      (if (i32.eq (local.get $b) (i32.const 92))
        (then (i32.store8 (i32.add (i32.const 44000) (local.get $i)) (i32.const 47))))
      (if (i32.lt_u (local.get $b) (i32.const 32))
        (then (i32.store8 (i32.add (i32.const 44000) (local.get $i)) (i32.const 32))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $loop)))

  ;; Answer a tool.execute for `readfile` (response prefix + id already
  ;; written): file content, or the errno on sandbox denial.
  (func $answer_readfile
    (local $len i32)
    (local.set $len (call $read_hello))
    (if (i32.ge_s (local.get $len) (i32.const 0))
      (then
        (call $sanitize (local.get $len))
        (call $write (i32.const 43200) (i32.const 45))   ;; ,"result":{"content":[{"type":"text","text":"
        (call $write (i32.const 44000) (local.get $len))
        (call $write (i32.const 43300) (i32.const 6)))   ;; "}]}} \n
      (else
        (call $write (i32.const 43400) (i32.const 64))   ;; ..."read failed: errno
        (call $write_u32 (i32.sub (i32.const 0) (local.get $len)))
        (call $write (i32.const 43300) (i32.const 6)))))

  (data (i32.const 40000) "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"3.0.0\",\"plugin\":{\"name\":\"hello-wasm-caps\"},\"capabilities\":{\"tools\":[{\"name\":\"ping\",\"description\":\"Answer with a pong from the WASM sandbox\",\"parameters\":{\"type\":\"object\",\"properties\":{}}},{\"name\":\"readfile\",\"description\":\"Read hello.txt from the preopened /data dir\",\"parameters\":{\"type\":\"object\",\"properties\":{}}}],\"commands\":[{\"name\":\"hello-wasm-caps\",\"description\":\"Greet from the sandboxed WASM plugin\"}]}}}\0a")
  (data (i32.const 41000) "{\"jsonrpc\":\"2.0\",\"id\":")
  (data (i32.const 41100) ",\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"pong from the WASM sandbox\"}]}}\0a")
  (data (i32.const 41200) ",\"result\":{\"ok\":true,\"note\":\"hello from the WASM sandbox\"}}\0a")
  (data (i32.const 41300) "\"method\":\"")
  (data (i32.const 41320) "tools/execute")
  (data (i32.const 41340) "\"id\":")
  (data (i32.const 41360) "readfile")
  (data (i32.const 43000) "hello.txt")
  (data (i32.const 43200) ",\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"")
  (data (i32.const 43300) "\"}]}}\0a")
  (data (i32.const 43400) ",\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"read failed: errno ")

  (func (export "_start")
    ;; Handshake: consume the `initialize` request, answer with the
    ;; capabilities result (id 1 — the host's first request).
    (drop (call $read_line))
    (call $write (i32.const 40000) (i32.const 465))
    ;; Main loop: answer requests until EOF (host shutdown closes stdin).
    (loop $main
      (if (call $read_line)
        (then
          (if (i32.ge_s (call $find (i32.const 41300) (i32.const 10)) (i32.const 0))
            (then
              (call $write (i32.const 41000) (i32.const 22))
              (call $write_u32 (call $parse_id))
              (if (i32.ge_s (call $find (i32.const 41360) (i32.const 8)) (i32.const 0))
                (then (call $answer_readfile))
                (else
                  (if (i32.ge_s (call $find (i32.const 41320) (i32.const 13)) (i32.const 0))
                    (then (call $write (i32.const 41100) (i32.const 77)))
                    (else (call $write (i32.const 41200) (i32.const 60))))))))
          (br $main))))
)
)

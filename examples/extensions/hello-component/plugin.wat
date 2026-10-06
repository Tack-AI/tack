;; hello-component: a tack plugin as a WIT component (tack:plugin@0.3.0),
;; hand-written component WAT — see protocol/wit/tack-plugin.wit for the
;; contract. No WASI, no handwritten JSON-RPC: the canonical ABI carries
;; JSON strings, so the whole guest is three functions returning static
;; payloads plus one `host.log` call per execute (a real guest would be
;; wit-bindgen-generated Rust/Go/JS/Python).
;;
;; Interface names carry the package VERSION — `tack:plugin/tools@0.3.0`
;; etc. — exactly the way component-model name mangling mangles a
;; versioned WIT package and real wit-bindgen/cargo-component guests
;; emit them (the host also accepts the bare names as a fallback for
;; hand-written guests; the carrier's spinner fixtures cover that path).
;;
;; Canonical-ABI cheat sheet for the signatures used here (spilled results
;; are written by the guest into its own memory, pointer returned):
;;   list() -> string                    linear layout: (ptr i32, len i32)
;;   execute(call: string) -> result<string,string>
;;     params flat: (ptr i32, len i32)
;;     result variant linear layout: (disc u8, pad, ptr i32 @4, len i32 @8)
;;   log(level: string, message: string)
;;     lowered core params: (level ptr, level len, msg ptr, msg len)
;; realloc is the bump allocator the ABI uses to lower string params.
(component
  ;; Import the host interface under its version-qualified name, like a
  ;; real SDK guest.
  (import "tack:plugin/host@0.3.0" (instance $host
    (export "log" (func (param "level" string) (param "message" string)))))

  ;; The canonical-ABI LOWER of `host.log` needs a memory (to read the
  ;; string params from) that must be instantiated BEFORE the lower, so
  ;; that memory lives in its own tiny module ($abi) — the main module
  ;; $m keeps its own memory for the lifted exports. The log strings the
  ;; execute function passes must therefore live in $abi's memory.
  (core module $abi
    (memory (export "memory") 1)
    (global $heap (mut i32) (i32.const 1024))
    (func (export "realloc") (param $old i32) (param $old_size i32) (param $align i32) (param $new_size i32) (result i32)
      (local $ptr i32)
      (local.set $ptr (global.get $heap))
      (global.set $heap (i32.add (local.get $ptr) (local.get $new_size)))
      (local.get $ptr))
    (data (i32.const 128) "info")
    (data (i32.const 256) "hello-component executed a tool")
  )
  (core instance $abi_i (instantiate $abi))
  (core func $log_lowered (canon lower (func $host "log") (memory (core memory $abi_i "memory")) (realloc (core func $abi_i "realloc"))))

  (core module $m
    (import "tack" "log" (func $log (param i32 i32 i32 i32)))
    (memory (export "memory") 2)
    (global $heap (mut i32) (i32.const 32768))

    ;; Canonical-ABI realloc: (old_ptr, old_size, align, new_size) -> ptr.
    (func (export "realloc") (param $old i32) (param $old_size i32) (param $align i32) (param $new_size i32) (result i32)
      (local $ptr i32)
      (local.set $ptr (global.get $heap))
      (global.set $heap (i32.add (local.get $ptr) (local.get $new_size)))
      (local.get $ptr))

    ;; tools.list() -> string: write the (ptr, len) pair at 4096, return it.
    (func (export "list") (result i32)
      (i32.store (i32.const 4096) (i32.const 8192))
      (i32.store (i32.const 4100) (i32.const 142))
      (i32.const 4096))

    ;; tools.execute(call) -> result<string,string>:
    ;; variant layout at 4096: disc u8 (0 = ok), ptr @4100, len @4104.
    ;; Emits one host.log line first: level "info" (4 bytes @128 in
    ;; $abi's memory), message (31 bytes @256 there) — exercising the
    ;; link-and-dispatch path of the host import.
    (func (export "execute") (param i32 i32) (result i32)
      (call $log (i32.const 128) (i32.const 4) (i32.const 256) (i32.const 31))
      (i32.store (i32.const 4096) (i32.const 0))
      (i32.store (i32.const 4100) (i32.const 12288))
      (i32.store (i32.const 4104) (i32.const 71))
      (i32.const 4096))

    ;; hooks.before-tool-call(call) -> result<string,string>: allow all.
    (func (export "before-tool-call") (param i32 i32) (result i32)
      (i32.store (i32.const 4096) (i32.const 0))
      (i32.store (i32.const 4100) (i32.const 16384))
      (i32.store (i32.const 4104) (i32.const 18))
      (i32.const 4096))

    (data (i32.const 8192) "[{\"name\":\"hello\",\"description\":\"Say hello from the component carrier\",\"parameters\":{\"type\":\"object\",\"properties\":{\"name\":{\"type\":\"string\"}}}}]")
    (data (i32.const 12288) "{\"content\":[{\"type\":\"text\",\"text\":\"hello from the component carrier\"}]}")
    (data (i32.const 16384) "{\"action\":\"allow\"}")
  )
  (core instance $i (instantiate $m
    (with "tack" (instance (export "log" (func $log_lowered))))))

  (func $list
    (result string)
    (canon lift (core func $i "list") (memory (core memory $i "memory")) (realloc (core func $i "realloc"))))
  (func $execute
    (param "call" string)
    (result (result string (error string)))
    (canon lift (core func $i "execute") (memory (core memory $i "memory")) (realloc (core func $i "realloc"))))
  (func $before_tool_call
    (param "call" string)
    (result (result string (error string)))
    (canon lift (core func $i "before-tool-call") (memory (core memory $i "memory")) (realloc (core func $i "realloc"))))

  (instance $tools
    (export "list" (func $list))
    (export "execute" (func $execute)))
  (instance $hooks
    (export "before-tool-call" (func $before_tool_call)))
  (export "tack:plugin/tools@0.3.0" (instance $tools))
  (export "tack:plugin/hooks@0.3.0" (instance $hooks))
)

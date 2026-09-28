;; hello-component: a tack plugin as a WIT component (tack:plugin@0.3.0),
;; hand-written component WAT — see protocol/wit/tack-plugin.wit for the
;; contract. No WASI, no handwritten JSON-RPC: the canonical ABI carries
;; JSON strings, so the whole guest is three functions returning static
;; payloads (a real guest would be wit-bindgen-generated Rust/Go/JS/Python).
;;
;; Canonical-ABI cheat sheet for the signatures used here (spilled results
;; are written by the guest into its own memory, pointer returned):
;;   list() -> string                    linear layout: (ptr i32, len i32)
;;   execute(call: string) -> result<string,string>
;;     params flat: (ptr i32, len i32)
;;     result variant linear layout: (disc u8, pad, ptr i32 @4, len i32 @8)
;; realloc is the bump allocator the ABI uses to lower string params.
(component
  (core module $m
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
    (func (export "execute") (param i32 i32) (result i32)
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
  (core instance $i (instantiate $m))

  (func $list
    (result string)
    (canon lift (core func $i "list") (memory $i "memory") (realloc (func $i "realloc"))))
  (func $execute
    (param "call" string)
    (result (result string (error string)))
    (canon lift (core func $i "execute") (memory $i "memory") (realloc (func $i "realloc"))))
  (func $before_tool_call
    (param "call" string)
    (result (result string (error string)))
    (canon lift (core func $i "before-tool-call") (memory $i "memory") (realloc (func $i "realloc"))))

  (instance $tools
    (export "list" (func $list))
    (export "execute" (func $execute)))
  (instance $hooks
    (export "before-tool-call" (func $before_tool_call)))
  (export "tack:plugin/tools" (instance $tools))
  (export "tack:plugin/hooks" (instance $hooks))
)

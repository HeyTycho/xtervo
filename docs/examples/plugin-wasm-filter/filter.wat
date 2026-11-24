;; xtervo Plugin API v3 WASM output-filter example.
;;
;; ABI (see docs/PLUGIN_V3.md):
;;   memory exported; max 256 pages
;;   alloc(len: i32) -> ptr: i32    input ptr must be >= 262144
;;   filter(ptr,len: i32) -> out_len: i32
;;   output bytes are written at memory[0..out_len]
;;
;; Pattern: literal `[xtervo-wasm]` (13 bytes) -> `WASM-FILTER-OK` (14 bytes).
;; Everything else is copied byte-for-byte.
(module
  ;; 8 pages = 512 KiB initially; alloc grows for large chunks up to 256 pages.
  (memory (export "memory") 8 256)

  ;; Pattern + replacement live above the output buffer, below the input base.
  (data (i32.const 65536) "[xtervo-wasm]")
  (data (i32.const 65552) "WASM-FILTER-OK")

  ;; Input buffer starts at 256 KiB: output (max 256 KiB) can never overwrite
  ;; bytes the filter has not read yet.
  (func (export "alloc") (param $len i32) (result i32)
    (local $need i32)
    (local $pages i32)
    (local.set $need
      (i32.div_u
        (i32.add
          (i32.add (local.get $len) (i32.const 262144))
          (i32.const 65535))
        (i32.const 65536)))
    (local.set $pages (memory.size))
    (if (i32.gt_u (local.get $need) (local.get $pages))
      (then
        (if (i32.eq
              (memory.grow (i32.sub (local.get $need) (local.get $pages)))
              (i32.const -1))
          (then unreachable))))
    (i32.const 262144))

  (func (export "filter") (param $in i32) (param $len i32) (result i32)
    (local $i i32)
    (local $o i32)
    (local $b i32)
    (local $match i32)

    (block $done
      (loop $next
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))

        ;; Is the next 13 bytes exactly [xtervo-wasm]?
        (if (i32.le_u (i32.add (local.get $i) (i32.const 13)) (local.get $len))
          (then
            (local.set $match (i32.const 1))
            (local.set $b (i32.const 0))
            (block $mismatch
              (loop $cmp
                (br_if $mismatch (i32.ge_u (local.get $b) (i32.const 13)))
                (if (i32.ne
                      (i32.load8_u
                        (i32.add
                          (local.get $in)
                          (i32.add (local.get $i) (local.get $b))))
                      (i32.load8_u (i32.add (i32.const 65536) (local.get $b))))
                  (then
                    (local.set $match (i32.const 0))
                    (br $mismatch)))
                (local.set $b (i32.add (local.get $b) (i32.const 1)))
                (br $cmp)))

            (if (local.get $match)
              (then
                (local.set $b (i32.const 0))
                (block $replace_done
                  (loop $replace
                    (br_if $replace_done (i32.ge_u (local.get $b) (i32.const 14)))
                    (i32.store8
                      (i32.add (local.get $o) (local.get $b))
                      (i32.load8_u (i32.add (i32.const 65552) (local.get $b))))
                    (local.set $b (i32.add (local.get $b) (i32.const 1)))
                    (br $replace)))
                (local.set $i (i32.add (local.get $i) (i32.const 13)))
                (local.set $o (i32.add (local.get $o) (i32.const 14)))
                (br $next)))))

        ;; No match: copy one byte and advance.
        (i32.store8
          (local.get $o)
          (i32.load8_u (i32.add (local.get $in) (local.get $i))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (local.set $o (i32.add (local.get $o) (i32.const 1)))
        (br $next)))

    (local.get $o)))

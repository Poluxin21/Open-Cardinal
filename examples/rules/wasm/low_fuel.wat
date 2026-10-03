;; The WebAssembly twin of the default Lua rule: SHUTDOWN when "fuel" < 70.
;; Assemble with `wat2wasm low_fuel.wat -o low_fuel.wasm` (or the `wat` crate) and drop the
;; .wasm next to your other rules. The ABI is documented in docs/RULES.md.
(module
  (memory (export "memory") 1)
  (data (i32.const 1024) "\"fuel\":\"")
  (data (i32.const 2048) "{\"action\":\"SHUTDOWN\",\"cmd_name\":\"EMERGENCY_CUTOFF\",\"priority\":1000,\"params\":{\"reason\":\"Overheating\"}}")

  (func (export "cardinal_alloc") (param i32) (result i32) (i32.const 8192))

  ;; index just past the first occurrence of the 8-byte needle at 1024, or -1
  (func $find (param $ptr i32) (param $len i32) (result i32)
    (local $i i32) (local $j i32)
    (block $notfound
      (loop $outer
        (br_if $notfound (i32.gt_s (i32.add (local.get $i) (i32.const 8)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch
              (i32.ne
                (i32.load8_u (i32.add (local.get $ptr) (i32.add (local.get $i) (local.get $j))))
                (i32.load8_u (i32.add (i32.const 1024) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (i32.const 8)))
            (return (i32.add (local.get $i) (i32.const 8)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const -1))

  (func (export "cardinal_evaluate") (param $ptr i32) (param $len i32) (result i64)
    (local $p i32) (local $v i32) (local $c i32)
    (local.set $p (call $find (local.get $ptr) (local.get $len)))
    (if (i32.lt_s (local.get $p) (i32.const 0)) (then (return (i64.const 0))))
    (block $done
      (loop $digits
        (local.set $c (i32.load8_u (i32.add (local.get $ptr) (local.get $p))))
        (br_if $done (i32.or (i32.lt_u (local.get $c) (i32.const 48)) (i32.gt_u (local.get $c) (i32.const 57))))
        (local.set $v (i32.add (i32.mul (local.get $v) (i32.const 10)) (i32.sub (local.get $c) (i32.const 48))))
        (local.set $p (i32.add (local.get $p) (i32.const 1)))
        (br $digits)))
    (if (i32.lt_u (local.get $v) (i32.const 70))
      (then (return (i64.or (i64.shl (i64.const 2048) (i64.const 32)) (i64.const 101)))))
    (i64.const 0)))

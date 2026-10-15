;; The core module of the test plugin, hand-written against the canonical ABI
;; of `wit/plugin.wit`. `regenerate.sh` wraps it into `fixture-plugin.wat`, the
;; component the tests load.
;;
;; Behavior (what the tests pin):
;; - metadata: name "fixture", description "test plugin", tool `echo`
;;   (requires a string `text`), command `hello` (hint `<text>`)
;; - on-session-start: log(info, "started"); set-status(sid, some("fixture ready"),
;;   ["panel row one", "panel row two"])
;; - review-tool-call: block "fixture blocks forbidden" when the tool is `forbidden`
;; - after-tool-call: some("fixture note")
;; - review-host-request: block "fixture: destructive command" when the request
;;   is `bash.run` and its data contains `rm -rf`; ask "fixture asks first"
;;   when it is `bash.run` and its data contains `ask-me`
;; - after-host-request: some("fixture after bash.run") for an ok `bash.run`
;; - prompt-context: block "fixture refuses" for the prompt `forbidden prompt`,
;;   otherwise context(some("fixture context"))
;; - call: `echo` -> ok(data); `trap` -> trap; `spin` -> loops forever;
;;   `grow` -> grows memory by 4096 pages; anything else -> err("unknown method")
;; - run-command: `hello` -> ok(args); anything else -> err("unknown command")
(module
  (import "prime-agent:plugin/host@0.2.0" "log" (func $log (param i32 i32 i32)))
  (import "prime-agent:plugin/host@0.2.0" "set-status"
    (func $set_status (param i32 i32 i32 i32 i32 i32 i32)))

  (memory (export "memory") 2)

  ;; Static strings.
  (data (i32.const 1024) "fixture")                        ;; 7
  (data (i32.const 1040) "test plugin")                    ;; 11
  (data (i32.const 1056) "started")                        ;; 7
  (data (i32.const 1072) "fixture ready")                  ;; 13
  (data (i32.const 1088) "panel row one")                  ;; 13
  (data (i32.const 1104) "panel row two")                  ;; 13
  (data (i32.const 1120) "forbidden")                      ;; 9
  (data (i32.const 1136) "fixture blocks forbidden")       ;; 24
  (data (i32.const 1168) "fixture note")                   ;; 12
  (data (i32.const 1184) "bash.run")                       ;; 8
  (data (i32.const 1200) "rm -rf")                         ;; 6
  (data (i32.const 1216) "fixture: destructive command")   ;; 28
  (data (i32.const 1248) "forbidden prompt")               ;; 16
  (data (i32.const 1280) "fixture refuses")                ;; 15
  (data (i32.const 1296) "fixture context")                ;; 15
  (data (i32.const 1312) "echo")                           ;; 4
  (data (i32.const 1320) "trap")                           ;; 4
  (data (i32.const 1328) "spin")                           ;; 4
  (data (i32.const 1336) "grow")                           ;; 4
  (data (i32.const 1344) "unknown method")                 ;; 14
  ;; The panel list: two (ptr, len) pairs.
  (data (i32.const 1408) "\40\04\00\00\0d\00\00\00\50\04\00\00\0d\00\00\00")
  (data (i32.const 1600) "fixture after bash.run")        ;; 22
  (data (i32.const 1632) "hello")                         ;; 5
  (data (i32.const 1640) "unknown command")               ;; 15
  (data (i32.const 1664) "Echo the arguments back")       ;; 23
  (data (i32.const 1696) "{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\"}},\"required\":[\"text\"]}");; 77
  (data (i32.const 1792) "Echo the arguments")            ;; 18
  (data (i32.const 1824) "<text>")                        ;; 6
  (data (i32.const 1840) "ask-me")                        ;; 6
  (data (i32.const 1848) "fixture asks first")            ;; 18
  ;; The tool list: one tool-spec (name, description, parameters-json).
  (data (i32.const 1872) "\20\05\00\00\04\00\00\00\80\06\00\00\17\00\00\00\a0\06\00\00\4d\00\00\00")
  ;; The command list: one command-spec (name, description, some(hint)).
  (data (i32.const 1904) "\60\06\00\00\05\00\00\00\00\07\00\00\12\00\00\00\01\00\00\00\20\07\00\00\06\00\00\00")

  ;; The return area every multi-value export writes into.
  (global $ret i32 (i32.const 512))
  ;; The bump allocator's next free byte.
  (global $heap (mut i32) (i32.const 65536))

  (func $cabi_realloc (export "cabi_realloc")
    (param $old i32) (param $old_size i32) (param $align i32) (param $size i32) (result i32)
    (local $ptr i32)
    (local.set $ptr
      (i32.and
        (i32.add (global.get $heap) (i32.sub (local.get $align) (i32.const 1)))
        (i32.xor (i32.sub (local.get $align) (i32.const 1)) (i32.const -1))))
    (global.set $heap (i32.add (local.get $ptr) (local.get $size)))
    (block $fits
      (br_if $fits (i32.le_u (global.get $heap) (i32.mul (memory.size) (i32.const 65536))))
      (drop (memory.grow
        (i32.add
          (i32.div_u (i32.sub (global.get $heap) (i32.mul (memory.size) (i32.const 65536)))
                     (i32.const 65536))
          (i32.const 1)))))
    (if (local.get $old_size)
      (then (memory.copy (local.get $ptr) (local.get $old) (local.get $old_size))))
    (local.get $ptr))

  ;; Byte equality of two strings.
  (func $eq (param $a i32) (param $alen i32) (param $b i32) (param $blen i32) (result i32)
    (local $i i32)
    (if (i32.ne (local.get $alen) (local.get $blen)) (then (return (i32.const 0))))
    (block $done
      (loop $next
        (br_if $done (i32.ge_u (local.get $i) (local.get $alen)))
        (if (i32.ne (i32.load8_u (i32.add (local.get $a) (local.get $i)))
                    (i32.load8_u (i32.add (local.get $b) (local.get $i))))
          (then (return (i32.const 0))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $next)))
    (i32.const 1))

  ;; Whether `hay` contains `needle`.
  (func $contains (param $hay i32) (param $haylen i32) (param $needle i32) (param $nlen i32)
    (result i32)
    (local $i i32)
    (block $done
      (loop $next
        (br_if $done (i32.gt_u (i32.add (local.get $i) (local.get $nlen)) (local.get $haylen)))
        (if (call $eq (i32.add (local.get $hay) (local.get $i)) (local.get $nlen)
                      (local.get $needle) (local.get $nlen))
          (then (return (i32.const 1))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $next)))
    (i32.const 0))

  ;; Write a two-word (ptr, len) payload behind a one-byte discriminant.
  (func $ret_variant (param $disc i32) (param $ptr i32) (param $len i32) (result i32)
    (i32.store8 (global.get $ret) (local.get $disc))
    (i32.store offset=4 (global.get $ret) (local.get $ptr))
    (i32.store offset=8 (global.get $ret) (local.get $len))
    (global.get $ret))

  ;; plugin-metadata: name(ptr, len) description(ptr, len) tools(ptr, len)
  ;; commands(ptr, len)
  (func (export "metadata") (result i32)
    (i32.store (global.get $ret) (i32.const 1024))
    (i32.store offset=4 (global.get $ret) (i32.const 7))
    (i32.store offset=8 (global.get $ret) (i32.const 1040))
    (i32.store offset=12 (global.get $ret) (i32.const 11))
    (i32.store offset=16 (global.get $ret) (i32.const 1872))
    (i32.store offset=20 (global.get $ret) (i32.const 1))
    (i32.store offset=24 (global.get $ret) (i32.const 1904))
    (i32.store offset=28 (global.get $ret) (i32.const 1))
    (global.get $ret))

  ;; session-info: sid(ptr, len) cwd(ptr, len) depth model(ptr, len)
  (func (export "on-session-start")
    (param $sid i32) (param $sidlen i32) (param i32 i32) (param i32) (param i32 i32)
    (call $log (i32.const 1) (i32.const 1056) (i32.const 7))
    (call $set_status
      (local.get $sid) (local.get $sidlen)
      (i32.const 1) (i32.const 1072) (i32.const 13)
      (i32.const 1408) (i32.const 2)))

  (func (export "on-agent-end") (param i32 i32))

  ;; sid(2) tool-call{id(2) name(2) args(2)}
  (func (export "review-tool-call")
    (param i32 i32) (param i32 i32) (param $name i32) (param $namelen i32) (param i32 i32)
    (result i32)
    (if (call $eq (local.get $name) (local.get $namelen) (i32.const 1120) (i32.const 9))
      (then (return (call $ret_variant (i32.const 2) (i32.const 1136) (i32.const 24)))))
    (call $ret_variant (i32.const 0) (i32.const 0) (i32.const 0)))

  ;; sid(2) tool-result{id(2) name(2) args(2) is-error text(2)}
  (func (export "after-tool-call")
    (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)
    (call $ret_variant (i32.const 1) (i32.const 1168) (i32.const 12)))

  ;; verdict: allow = 0, ask(string) = 1, block(string) = 2.
  ;; sid(2) host-request{type(2) data(2)}
  (func (export "review-host-request")
    (param i32 i32) (param $rt i32) (param $rtlen i32) (param $data i32) (param $datalen i32)
    (result i32)
    (if (i32.and
          (call $eq (local.get $rt) (local.get $rtlen) (i32.const 1184) (i32.const 8))
          (call $contains (local.get $data) (local.get $datalen) (i32.const 1200) (i32.const 6)))
      (then (return (call $ret_variant (i32.const 2) (i32.const 1216) (i32.const 28)))))
    (if (i32.and
          (call $eq (local.get $rt) (local.get $rtlen) (i32.const 1184) (i32.const 8))
          (call $contains (local.get $data) (local.get $datalen) (i32.const 1840) (i32.const 6)))
      (then (return (call $ret_variant (i32.const 1) (i32.const 1848) (i32.const 18)))))
    (call $ret_variant (i32.const 0) (i32.const 0) (i32.const 0)))

  ;; sid(2) host-request{type(2) data(2)} outcome: result<string, string>
  ;; (disc, ptr, len) -> option<string>
  (func (export "after-host-request")
    (param i32 i32) (param $rt i32) (param $rtlen i32) (param i32 i32)
    (param $outcome i32) (param i32 i32)
    (result i32)
    (if (i32.and
          (i32.eqz (local.get $outcome))
          (call $eq (local.get $rt) (local.get $rtlen) (i32.const 1184) (i32.const 8)))
      (then (return (call $ret_variant (i32.const 1) (i32.const 1600) (i32.const 22)))))
    (call $ret_variant (i32.const 0) (i32.const 0) (i32.const 0)))

  ;; prompt-decision: context(option<string>) = 0, block(string) = 1.
  ;; Layout: disc@0, payload@4 (option: disc@4, ptr@8, len@12; string: ptr@4, len@8).
  (func (export "prompt-context")
    (param i32 i32) (param $prompt i32) (param $promptlen i32) (result i32)
    (if (call $eq (local.get $prompt) (local.get $promptlen) (i32.const 1248) (i32.const 16))
      (then (return (call $ret_variant (i32.const 1) (i32.const 1280) (i32.const 15)))))
    (i32.store8 (global.get $ret) (i32.const 0))
    (i32.store8 offset=4 (global.get $ret) (i32.const 1))
    (i32.store offset=8 (global.get $ret) (i32.const 1296))
    (i32.store offset=12 (global.get $ret) (i32.const 15))
    (global.get $ret))

  ;; sid(2) method(2) data(2) -> result<string, string>
  (func (export "call")
    (param i32 i32) (param $m i32) (param $mlen i32) (param $data i32) (param $datalen i32)
    (result i32)
    (if (call $eq (local.get $m) (local.get $mlen) (i32.const 1312) (i32.const 4))
      (then (return (call $ret_variant (i32.const 0) (local.get $data) (local.get $datalen)))))
    (if (call $eq (local.get $m) (local.get $mlen) (i32.const 1320) (i32.const 4))
      (then unreachable))
    (if (call $eq (local.get $m) (local.get $mlen) (i32.const 1328) (i32.const 4))
      (then (loop $forever (br $forever))))
    (if (call $eq (local.get $m) (local.get $mlen) (i32.const 1336) (i32.const 4))
      (then
        (if (i32.eq (memory.grow (i32.const 4096)) (i32.const -1))
          (then unreachable))))
    (call $ret_variant (i32.const 1) (i32.const 1344) (i32.const 14)))

  ;; sid(2) name(2) args(2) -> result<string, string>
  (func (export "run-command")
    (param i32 i32) (param $name i32) (param $namelen i32) (param $args i32) (param $argslen i32)
    (result i32)
    (if (call $eq (local.get $name) (local.get $namelen) (i32.const 1632) (i32.const 5))
      (then (return (call $ret_variant (i32.const 0) (local.get $args) (local.get $argslen)))))
    (call $ret_variant (i32.const 1) (i32.const 1640) (i32.const 15)))
)

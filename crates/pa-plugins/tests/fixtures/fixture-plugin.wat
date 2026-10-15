(component
  (type $ty-prime-agent:plugin/host@0.2.0 (;0;)
    (instance
      (type (;0;) (enum "debug" "info" "warn" "error"))
      (export (;1;) "level" (type (eq 0)))
      (type (;2;) (func (param "level" 1) (param "message" string)))
      (export (;0;) "log" (func (type 2)))
      (type (;3;) (option string))
      (type (;4;) (list string))
      (type (;5;) (func (param "session-id" string) (param "line" 3) (param "panel" 4)))
      (export (;1;) "set-status" (func (type 5)))
    )
  )
  (import "prime-agent:plugin/host@0.2.0" (instance $prime-agent:plugin/host@0.2.0 (;0;) (type $ty-prime-agent:plugin/host@0.2.0)))
  (type $ty-prime-agent:plugin/types@0.2.0 (;1;)
    (instance
      (type (;0;) (record (field "session-id" string) (field "cwd" string) (field "rlm-depth" u32) (field "model" string)))
      (export (;1;) "session-info" (type (eq 0)))
      (type (;2;) (record (field "tool-call-id" string) (field "tool-name" string) (field "args-json" string)))
      (export (;3;) "tool-call" (type (eq 2)))
      (type (;4;) (record (field "tool-call-id" string) (field "tool-name" string) (field "args-json" string) (field "is-error" bool) (field "text" string)))
      (export (;5;) "tool-result" (type (eq 4)))
      (type (;6;) (record (field "request-type" string) (field "data-json" string)))
      (export (;7;) "host-request" (type (eq 6)))
      (type (;8;) (variant (case "allow") (case "ask" string) (case "block" string)))
      (export (;9;) "verdict" (type (eq 8)))
      (type (;10;) (option string))
      (type (;11;) (variant (case "context" 10) (case "block" string)))
      (export (;12;) "prompt-decision" (type (eq 11)))
      (type (;13;) (record (field "name" string) (field "description" string) (field "parameters-json" string)))
      (export (;14;) "tool-spec" (type (eq 13)))
      (type (;15;) (record (field "name" string) (field "description" string) (field "argument-hint" 10)))
      (export (;16;) "command-spec" (type (eq 15)))
      (type (;17;) (list 14))
      (type (;18;) (list 16))
      (type (;19;) (record (field "name" string) (field "description" string) (field "tools" 17) (field "commands" 18)))
      (export (;20;) "plugin-metadata" (type (eq 19)))
    )
  )
  (import "prime-agent:plugin/types@0.2.0" (instance $prime-agent:plugin/types@0.2.0 (;1;) (type $ty-prime-agent:plugin/types@0.2.0)))
  (alias export $prime-agent:plugin/types@0.2.0 "session-info" (type $session-info (;2;)))
  (import "session-info" (type $"#type3 session-info" (@name "session-info") (;3;) (eq $session-info)))
  (alias export $prime-agent:plugin/types@0.2.0 "tool-call" (type $tool-call (;4;)))
  (import "tool-call" (type $"#type5 tool-call" (@name "tool-call") (;5;) (eq $tool-call)))
  (alias export $prime-agent:plugin/types@0.2.0 "tool-result" (type $tool-result (;6;)))
  (import "tool-result" (type $"#type7 tool-result" (@name "tool-result") (;7;) (eq $tool-result)))
  (alias export $prime-agent:plugin/types@0.2.0 "host-request" (type $host-request (;8;)))
  (import "host-request" (type $"#type9 host-request" (@name "host-request") (;9;) (eq $host-request)))
  (alias export $prime-agent:plugin/types@0.2.0 "verdict" (type $verdict (;10;)))
  (import "verdict" (type $"#type11 verdict" (@name "verdict") (;11;) (eq $verdict)))
  (alias export $prime-agent:plugin/types@0.2.0 "prompt-decision" (type $prompt-decision (;12;)))
  (import "prompt-decision" (type $"#type13 prompt-decision" (@name "prompt-decision") (;13;) (eq $prompt-decision)))
  (alias export $prime-agent:plugin/types@0.2.0 "plugin-metadata" (type $plugin-metadata (;14;)))
  (import "plugin-metadata" (type $"#type15 plugin-metadata" (@name "plugin-metadata") (;15;) (eq $plugin-metadata)))
  (core module $main (;0;)
    (type (;0;) (func (param i32 i32 i32)))
    (type (;1;) (func (param i32 i32 i32 i32 i32 i32 i32)))
    (type (;2;) (func (param i32 i32 i32 i32) (result i32)))
    (type (;3;) (func (param i32 i32 i32) (result i32)))
    (type (;4;) (func (result i32)))
    (type (;5;) (func (param i32 i32)))
    (type (;6;) (func (param i32 i32 i32 i32 i32 i32 i32 i32) (result i32)))
    (type (;7;) (func (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)))
    (type (;8;) (func (param i32 i32 i32 i32 i32 i32) (result i32)))
    (type (;9;) (func (param i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)))
    (import "prime-agent:plugin/host@0.2.0" "log" (func $log (;0;) (type 0)))
    (import "prime-agent:plugin/host@0.2.0" "set-status" (func $set_status (;1;) (type 1)))
    (memory (;0;) 2)
    (global $ret (;0;) i32 i32.const 512)
    (global $heap (;1;) (mut i32) i32.const 65536)
    (export "memory" (memory 0))
    (export "cabi_realloc" (func $cabi_realloc))
    (export "metadata" (func 6))
    (export "on-session-start" (func 7))
    (export "on-agent-end" (func 8))
    (export "review-tool-call" (func 9))
    (export "after-tool-call" (func 10))
    (export "review-host-request" (func 11))
    (export "after-host-request" (func 12))
    (export "prompt-context" (func 13))
    (export "call" (func 14))
    (export "run-command" (func 15))
    (func $cabi_realloc (;2;) (type 2) (param $old i32) (param $old_size i32) (param $align i32) (param $size i32) (result i32)
      (local $ptr i32)
      global.get $heap
      local.get $align
      i32.const 1
      i32.sub
      i32.add
      local.get $align
      i32.const 1
      i32.sub
      i32.const -1
      i32.xor
      i32.and
      local.set $ptr
      local.get $ptr
      local.get $size
      i32.add
      global.set $heap
      block $fits
        global.get $heap
        memory.size
        i32.const 65536
        i32.mul
        i32.le_u
        br_if $fits
        global.get $heap
        memory.size
        i32.const 65536
        i32.mul
        i32.sub
        i32.const 65536
        i32.div_u
        i32.const 1
        i32.add
        memory.grow
        drop
      end
      local.get $old_size
      if ;; label = @1
        local.get $ptr
        local.get $old
        local.get $old_size
        memory.copy
      end
      local.get $ptr
    )
    (func $eq (;3;) (type 2) (param $a i32) (param $alen i32) (param $b i32) (param $blen i32) (result i32)
      (local $i i32)
      local.get $alen
      local.get $blen
      i32.ne
      if ;; label = @1
        i32.const 0
        return
      end
      block $done
        loop $next
          local.get $i
          local.get $alen
          i32.ge_u
          br_if $done
          local.get $a
          local.get $i
          i32.add
          i32.load8_u
          local.get $b
          local.get $i
          i32.add
          i32.load8_u
          i32.ne
          if ;; label = @3
            i32.const 0
            return
          end
          local.get $i
          i32.const 1
          i32.add
          local.set $i
          br $next
        end
      end
      i32.const 1
    )
    (func $contains (;4;) (type 2) (param $hay i32) (param $haylen i32) (param $needle i32) (param $nlen i32) (result i32)
      (local $i i32)
      block $done
        loop $next
          local.get $i
          local.get $nlen
          i32.add
          local.get $haylen
          i32.gt_u
          br_if $done
          local.get $hay
          local.get $i
          i32.add
          local.get $nlen
          local.get $needle
          local.get $nlen
          call $eq
          if ;; label = @3
            i32.const 1
            return
          end
          local.get $i
          i32.const 1
          i32.add
          local.set $i
          br $next
        end
      end
      i32.const 0
    )
    (func $ret_variant (;5;) (type 3) (param $disc i32) (param $ptr i32) (param $len i32) (result i32)
      global.get $ret
      local.get $disc
      i32.store8
      global.get $ret
      local.get $ptr
      i32.store offset=4
      global.get $ret
      local.get $len
      i32.store offset=8
      global.get $ret
    )
    (func (;6;) (type 4) (result i32)
      global.get $ret
      i32.const 1024
      i32.store
      global.get $ret
      i32.const 7
      i32.store offset=4
      global.get $ret
      i32.const 1040
      i32.store offset=8
      global.get $ret
      i32.const 11
      i32.store offset=12
      global.get $ret
      i32.const 1872
      i32.store offset=16
      global.get $ret
      i32.const 1
      i32.store offset=20
      global.get $ret
      i32.const 1904
      i32.store offset=24
      global.get $ret
      i32.const 1
      i32.store offset=28
      global.get $ret
    )
    (func (;7;) (type 1) (param $sid i32) (param $sidlen i32) (param i32 i32 i32 i32 i32)
      i32.const 1
      i32.const 1056
      i32.const 7
      call $log
      local.get $sid
      local.get $sidlen
      i32.const 1
      i32.const 1072
      i32.const 13
      i32.const 1408
      i32.const 2
      call $set_status
    )
    (func (;8;) (type 5) (param i32 i32))
    (func (;9;) (type 6) (param i32 i32 i32 i32) (param $name i32) (param $namelen i32) (param i32 i32) (result i32)
      local.get $name
      local.get $namelen
      i32.const 1120
      i32.const 9
      call $eq
      if ;; label = @1
        i32.const 2
        i32.const 1136
        i32.const 24
        call $ret_variant
        return
      end
      i32.const 0
      i32.const 0
      i32.const 0
      call $ret_variant
    )
    (func (;10;) (type 7) (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)
      i32.const 1
      i32.const 1168
      i32.const 12
      call $ret_variant
    )
    (func (;11;) (type 8) (param i32 i32) (param $rt i32) (param $rtlen i32) (param $data i32) (param $datalen i32) (result i32)
      local.get $rt
      local.get $rtlen
      i32.const 1184
      i32.const 8
      call $eq
      local.get $data
      local.get $datalen
      i32.const 1200
      i32.const 6
      call $contains
      i32.and
      if ;; label = @1
        i32.const 2
        i32.const 1216
        i32.const 28
        call $ret_variant
        return
      end
      local.get $rt
      local.get $rtlen
      i32.const 1184
      i32.const 8
      call $eq
      local.get $data
      local.get $datalen
      i32.const 1840
      i32.const 6
      call $contains
      i32.and
      if ;; label = @1
        i32.const 1
        i32.const 1848
        i32.const 18
        call $ret_variant
        return
      end
      i32.const 0
      i32.const 0
      i32.const 0
      call $ret_variant
    )
    (func (;12;) (type 9) (param i32 i32) (param $rt i32) (param $rtlen i32) (param i32 i32) (param $outcome i32) (param i32 i32) (result i32)
      local.get $outcome
      i32.eqz
      local.get $rt
      local.get $rtlen
      i32.const 1184
      i32.const 8
      call $eq
      i32.and
      if ;; label = @1
        i32.const 1
        i32.const 1600
        i32.const 22
        call $ret_variant
        return
      end
      i32.const 0
      i32.const 0
      i32.const 0
      call $ret_variant
    )
    (func (;13;) (type 2) (param i32 i32) (param $prompt i32) (param $promptlen i32) (result i32)
      local.get $prompt
      local.get $promptlen
      i32.const 1248
      i32.const 16
      call $eq
      if ;; label = @1
        i32.const 1
        i32.const 1280
        i32.const 15
        call $ret_variant
        return
      end
      global.get $ret
      i32.const 0
      i32.store8
      global.get $ret
      i32.const 1
      i32.store8 offset=4
      global.get $ret
      i32.const 1296
      i32.store offset=8
      global.get $ret
      i32.const 15
      i32.store offset=12
      global.get $ret
    )
    (func (;14;) (type 8) (param i32 i32) (param $m i32) (param $mlen i32) (param $data i32) (param $datalen i32) (result i32)
      local.get $m
      local.get $mlen
      i32.const 1312
      i32.const 4
      call $eq
      if ;; label = @1
        i32.const 0
        local.get $data
        local.get $datalen
        call $ret_variant
        return
      end
      local.get $m
      local.get $mlen
      i32.const 1320
      i32.const 4
      call $eq
      if ;; label = @1
        unreachable
      end
      local.get $m
      local.get $mlen
      i32.const 1328
      i32.const 4
      call $eq
      if ;; label = @1
        loop $forever
          br $forever
        end
      end
      local.get $m
      local.get $mlen
      i32.const 1336
      i32.const 4
      call $eq
      if ;; label = @1
        i32.const 4096
        memory.grow
        i32.const -1
        i32.eq
        if ;; label = @2
          unreachable
        end
      end
      i32.const 1
      i32.const 1344
      i32.const 14
      call $ret_variant
    )
    (func (;15;) (type 8) (param i32 i32) (param $name i32) (param $namelen i32) (param $args i32) (param $argslen i32) (result i32)
      local.get $name
      local.get $namelen
      i32.const 1632
      i32.const 5
      call $eq
      if ;; label = @1
        i32.const 0
        local.get $args
        local.get $argslen
        call $ret_variant
        return
      end
      i32.const 1
      i32.const 1640
      i32.const 15
      call $ret_variant
    )
    (data (;0;) (i32.const 1024) "fixture")
    (data (;1;) (i32.const 1040) "test plugin")
    (data (;2;) (i32.const 1056) "started")
    (data (;3;) (i32.const 1072) "fixture ready")
    (data (;4;) (i32.const 1088) "panel row one")
    (data (;5;) (i32.const 1104) "panel row two")
    (data (;6;) (i32.const 1120) "forbidden")
    (data (;7;) (i32.const 1136) "fixture blocks forbidden")
    (data (;8;) (i32.const 1168) "fixture note")
    (data (;9;) (i32.const 1184) "bash.run")
    (data (;10;) (i32.const 1200) "rm -rf")
    (data (;11;) (i32.const 1216) "fixture: destructive command")
    (data (;12;) (i32.const 1248) "forbidden prompt")
    (data (;13;) (i32.const 1280) "fixture refuses")
    (data (;14;) (i32.const 1296) "fixture context")
    (data (;15;) (i32.const 1312) "echo")
    (data (;16;) (i32.const 1320) "trap")
    (data (;17;) (i32.const 1328) "spin")
    (data (;18;) (i32.const 1336) "grow")
    (data (;19;) (i32.const 1344) "unknown method")
    (data (;20;) (i32.const 1408) "@\04\00\00\0d\00\00\00P\04\00\00\0d\00\00\00")
    (data (;21;) (i32.const 1600) "fixture after bash.run")
    (data (;22;) (i32.const 1632) "hello")
    (data (;23;) (i32.const 1640) "unknown command")
    (data (;24;) (i32.const 1664) "Echo the arguments back")
    (data (;25;) (i32.const 1696) "{\22type\22:\22object\22,\22properties\22:{\22text\22:{\22type\22:\22string\22}},\22required\22:[\22text\22]}")
    (data (;26;) (i32.const 1792) "Echo the arguments")
    (data (;27;) (i32.const 1824) "<text>")
    (data (;28;) (i32.const 1840) "ask-me")
    (data (;29;) (i32.const 1848) "fixture asks first")
    (data (;30;) (i32.const 1872) " \05\00\00\04\00\00\00\80\06\00\00\17\00\00\00\a0\06\00\00M\00\00\00")
    (data (;31;) (i32.const 1904) "`\06\00\00\05\00\00\00\00\07\00\00\12\00\00\00\01\00\00\00 \07\00\00\06\00\00\00")
    (@producers
      (processed-by "wit-component" "0.256.0")
    )
  )
  (core module $wit-component-shim-module (;1;)
    (type (;0;) (func (param i32 i32 i32)))
    (type (;1;) (func (param i32 i32 i32 i32 i32 i32 i32)))
    (table (;0;) 2 2 funcref)
    (export "0" (func 0))
    (export "1" (func 1))
    (export "$imports" (table 0))
    (func (;0;) (type 0) (param i32 i32 i32)
      local.get 0
      local.get 1
      local.get 2
      i32.const 0
      call_indirect (type 0)
    )
    (func (;1;) (type 1) (param i32 i32 i32 i32 i32 i32 i32)
      local.get 0
      local.get 1
      local.get 2
      local.get 3
      local.get 4
      local.get 5
      local.get 6
      i32.const 1
      call_indirect (type 1)
    )
    (@producers
      (processed-by "wit-component" "0.256.0")
    )
  )
  (core module $wit-component-fixup (;2;)
    (type (;0;) (func (param i32 i32 i32)))
    (type (;1;) (func (param i32 i32 i32 i32 i32 i32 i32)))
    (import "" "0" (func (;0;) (type 0)))
    (import "" "1" (func (;1;) (type 1)))
    (import "" "$imports" (table (;0;) 2 2 funcref))
    (elem (;0;) (i32.const 0) func 0 1)
    (@producers
      (processed-by "wit-component" "0.256.0")
    )
  )
  (core instance $wit-component-shim-instance (;0;) (instantiate $wit-component-shim-module))
  (alias core export $wit-component-shim-instance "0" (core func $indirect-prime-agent:plugin/host@0.2.0-log (;0;)))
  (alias core export $wit-component-shim-instance "1" (core func $indirect-prime-agent:plugin/host@0.2.0-set-status (;1;)))
  (core instance $prime-agent:plugin/host@0.2.0 (;1;)
    (export "log" (func $indirect-prime-agent:plugin/host@0.2.0-log))
    (export "set-status" (func $indirect-prime-agent:plugin/host@0.2.0-set-status))
  )
  (core instance $main (;2;) (instantiate $main
      (with "prime-agent:plugin/host@0.2.0" (instance $prime-agent:plugin/host@0.2.0))
    )
  )
  (alias core export $main "memory" (core memory $memory (;0;)))
  (alias core export $wit-component-shim-instance "$imports" (core table $"shim table" (;0;)))
  (alias export $prime-agent:plugin/host@0.2.0 "log" (func $log (;0;)))
  (alias core export $main "cabi_realloc" (core func $realloc (;2;)))
  (core func $"#core-func3 indirect-prime-agent:plugin/host@0.2.0-log" (@name "indirect-prime-agent:plugin/host@0.2.0-log") (;3;) (canon lower (func $log) (memory $memory) string-encoding=utf8))
  (alias export $prime-agent:plugin/host@0.2.0 "set-status" (func $set-status (;1;)))
  (core func $"#core-func4 indirect-prime-agent:plugin/host@0.2.0-set-status" (@name "indirect-prime-agent:plugin/host@0.2.0-set-status") (;4;) (canon lower (func $set-status) (memory $memory) string-encoding=utf8))
  (core instance $fixup-args (;3;)
    (export "$imports" (table $"shim table"))
    (export "0" (func $"#core-func3 indirect-prime-agent:plugin/host@0.2.0-log"))
    (export "1" (func $"#core-func4 indirect-prime-agent:plugin/host@0.2.0-set-status"))
  )
  (core instance $fixup (;4;) (instantiate $wit-component-fixup
      (with "" (instance $fixup-args))
    )
  )
  (type (;16;) (func (result $"#type15 plugin-metadata")))
  (alias core export $main "metadata" (core func $metadata (;5;)))
  (func $metadata (;2;) (type 16) (canon lift (core func $metadata) (memory $memory) string-encoding=utf8))
  (export $"#func3 metadata" (@name "metadata") (;3;) "metadata" (func $metadata))
  (type (;17;) (func (param "session" $"#type3 session-info")))
  (alias core export $main "on-session-start" (core func $on-session-start (;6;)))
  (func $on-session-start (;4;) (type 17) (canon lift (core func $on-session-start) (memory $memory) (realloc $realloc) string-encoding=utf8))
  (export $"#func5 on-session-start" (@name "on-session-start") (;5;) "on-session-start" (func $on-session-start))
  (type (;18;) (func (param "session-id" string)))
  (alias core export $main "on-agent-end" (core func $on-agent-end (;7;)))
  (func $on-agent-end (;6;) (type 18) (canon lift (core func $on-agent-end) (memory $memory) (realloc $realloc) string-encoding=utf8))
  (export $"#func7 on-agent-end" (@name "on-agent-end") (;7;) "on-agent-end" (func $on-agent-end))
  (type (;19;) (func (param "session-id" string) (param "call" $"#type5 tool-call") (result $"#type11 verdict")))
  (alias core export $main "review-tool-call" (core func $review-tool-call (;8;)))
  (func $review-tool-call (;8;) (type 19) (canon lift (core func $review-tool-call) (memory $memory) (realloc $realloc) string-encoding=utf8))
  (export $"#func9 review-tool-call" (@name "review-tool-call") (;9;) "review-tool-call" (func $review-tool-call))
  (type (;20;) (option string))
  (type (;21;) (func (param "session-id" string) (param "outcome" $"#type7 tool-result") (result 20)))
  (alias core export $main "after-tool-call" (core func $after-tool-call (;9;)))
  (func $after-tool-call (;10;) (type 21) (canon lift (core func $after-tool-call) (memory $memory) (realloc $realloc) string-encoding=utf8))
  (export $"#func11 after-tool-call" (@name "after-tool-call") (;11;) "after-tool-call" (func $after-tool-call))
  (type (;22;) (func (param "session-id" string) (param "request" $"#type9 host-request") (result $"#type11 verdict")))
  (alias core export $main "review-host-request" (core func $review-host-request (;10;)))
  (func $review-host-request (;12;) (type 22) (canon lift (core func $review-host-request) (memory $memory) (realloc $realloc) string-encoding=utf8))
  (export $"#func13 review-host-request" (@name "review-host-request") (;13;) "review-host-request" (func $review-host-request))
  (type (;23;) (result string (error string)))
  (type (;24;) (func (param "session-id" string) (param "request" $"#type9 host-request") (param "outcome" 23) (result 20)))
  (alias core export $main "after-host-request" (core func $after-host-request (;11;)))
  (func $after-host-request (;14;) (type 24) (canon lift (core func $after-host-request) (memory $memory) (realloc $realloc) string-encoding=utf8))
  (export $"#func15 after-host-request" (@name "after-host-request") (;15;) "after-host-request" (func $after-host-request))
  (type (;25;) (func (param "session-id" string) (param "prompt" string) (result $"#type13 prompt-decision")))
  (alias core export $main "prompt-context" (core func $prompt-context (;12;)))
  (func $prompt-context (;16;) (type 25) (canon lift (core func $prompt-context) (memory $memory) (realloc $realloc) string-encoding=utf8))
  (export $"#func17 prompt-context" (@name "prompt-context") (;17;) "prompt-context" (func $prompt-context))
  (type (;26;) (func (param "session-id" string) (param "method" string) (param "data-json" string) (result 23)))
  (alias core export $main "call" (core func $call (;13;)))
  (func $call (;18;) (type 26) (canon lift (core func $call) (memory $memory) (realloc $realloc) string-encoding=utf8))
  (export $"#func19 call" (@name "call") (;19;) "call" (func $call))
  (type (;27;) (func (param "session-id" string) (param "name" string) (param "args" string) (result 23)))
  (alias core export $main "run-command" (core func $run-command (;14;)))
  (func $run-command (;20;) (type 27) (canon lift (core func $run-command) (memory $memory) (realloc $realloc) string-encoding=utf8))
  (export $"#func21 run-command" (@name "run-command") (;21;) "run-command" (func $run-command))
  (@producers
    (processed-by "wit-component" "0.256.0")
  )
)

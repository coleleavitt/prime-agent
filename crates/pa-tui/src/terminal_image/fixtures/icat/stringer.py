# Runs kitty's own gen/go_code.py `stringify_file` (extracted verbatim with ast)
# over the copied command.go, producing command_stringer_generated.go.
import ast, sys, io, contextlib, re, os, shlex, argparse, typing
K=os.environ['KITTY']
want={'stringify_file':'gen/go_code.py','enum_parser':'gen/go_code.py','replace_if_needed':'gen/go_code.py','serialize_as_go_string':'kitty/simple_cli_definitions.py'}
code=[]
for name,f in want.items():
    s=open(os.path.join(K,f)).read()
    for n in ast.parse(s).body:
        if isinstance(n,ast.FunctionDef) and n.name==name:
            seg=ast.get_source_segment(s,n)
            decos=''.join('@'+ast.get_source_segment(s,d)+'\n' for d in n.decorator_list)
            code.append(decos+seg)
import functools
g={'__file__':'go_code.py','changed':[],'suppress':contextlib.suppress,'lru_cache':functools.lru_cache,'contextmanager':contextlib.contextmanager,'re':re,'os':os,'shlex':shlex,'argparse':argparse,'contextlib':contextlib,'io':io,'sys':sys}
g.update({k:getattr(typing,k) for k in ('Iterator','Any','Optional')})
exec('\n\n'.join(code), g)
g['stringify_file']('graphics/command.go')

#!/usr/bin/env python3
"""Check real stdio MCP negotiation/tools. --live also reads public course/slide."""
import argparse
import json
import os
import selectors
import subprocess
import time
import tempfile
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', default=str(Path(__file__).resolve().parents[1] / 'target/debug/ulearn-mcp'))
parser.add_argument('--live', action='store_true')
args = parser.parse_args()
env = dict(os.environ)
# Public smoke never reads local credentials or student records.
for name in ('ULEARN_TOKEN', 'ULEARN_TOKEN_FILE', 'ULEARN_API_URL'):
    env.pop(name, None)
smoke_config = tempfile.TemporaryDirectory(prefix='ulearn-smoke-')
env['XDG_CONFIG_HOME'] = smoke_config.name
p = subprocess.Popen([args.binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
selector = selectors.DefaultSelector()
selector.register(p.stdout, selectors.EVENT_READ)
buffer = b''
seq = 0

def send(message):
    p.stdin.write(json.dumps(dict(jsonrpc='2.0', **message)).encode() + b'\n')
    p.stdin.flush()

def call(method, params):
    global seq, buffer
    seq += 1
    send(dict(id=seq, method=method, params=params))
    deadline = time.monotonic() + 60
    while True:
        while b'\n' not in buffer:
            if not selector.select(max(0, deadline - time.monotonic())):
                raise RuntimeError('MCP response timeout')
            chunk = os.read(p.stdout.fileno(), 65536)
            if not chunk:
                raise RuntimeError('MCP exited: ' + p.stderr.read().decode())
            buffer += chunk
        line, buffer = buffer.split(b'\n', 1)
        value = json.loads(line)
        if value.get('id') == seq:
            assert 'error' not in value, value
            return value['result']

try:
    init = call('initialize', dict(protocolVersion='2025-06-18', capabilities={}, clientInfo=dict(name='ulearn-smoke', version='1')))
    send(dict(method='notifications/initialized'))
    tools = call('tools/list', {})['tools']
    names = {t['name'] for t in tools}
    assert {'get_course','get_slide','get_submissions','list_review_queue','get_review_context','add_review_comment','reply_to_review','set_review_score'} <= names
    for tool in tools:
        if tool['name'] in {'add_review_comment','reply_to_review','set_review_score'}:
            assert tool['annotations']['readOnlyHint'] is False
    if args.live:
        result = call('tools/call', dict(name='get_course', arguments={'course_id':'nand2tetris'}))
        assert not result.get('isError'), result
        data = result.get('structuredContent') or json.loads(result['content'][0]['text'])
        assert data['id'].lower() == 'nand2tetris'
        slide = next(s for u in data['units'] for s in u['slides'] if s['type']=='exercise')
        result = call('tools/call', dict(name='get_slide', arguments={'course_id':'nand2tetris','slide_id':slide['id']}))
        assert not result.get('isError'), result
        content = result.get('structuredContent') or json.loads(result['content'][0]['text'])
        assert content.get('blocks'), content
        print(f"Live public read OK: {len(data['units'])} topics; exercise content fetched")
    print(f"MCP stdio OK: protocol {init['protocolVersion']}; {len(tools)} tools")
finally:
    p.stdin.close()
    try:
        p.wait(timeout=3)
    except subprocess.TimeoutExpired:
        p.terminate()
        p.wait(timeout=3)
    selector.close()
    smoke_config.cleanup()

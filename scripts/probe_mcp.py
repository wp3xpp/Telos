#!/usr/bin/env python3
"""Isolated phase-0 MCP fixture. No network, production access, or credentials."""
import argparse
import json
import os
from pathlib import Path
import sys
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--evidence-dir', type=Path, required=True)
    parser.add_argument('--nonce', required=True)
    args = parser.parse_args()
    args.evidence_dir.mkdir(parents=True, exist_ok=True)
    (args.evidence_dir / 'mcp.pid').write_text(str(os.getpid()))
    for line in sys.stdin:
        req = json.loads(line)
        method = req.get('method')
        if 'id' not in req:
            continue
        response = {'jsonrpc': '2.0', 'id': req['id']}
        if method == 'initialize':
            result = {
                'protocolVersion': req['params']['protocolVersion'],
                'capabilities': {'tools': {}},
                'serverInfo': {'name': 'telos-phase0', 'version': '0.1.0'},
            }
        elif method == 'ping':
            result = {}
        elif method == 'tools/list':
            result = {'tools': [{
                'name': name,
                'description': desc,
                'inputSchema': {'type': 'object', 'properties': {}, 'additionalProperties': False},
                'annotations': {'readOnlyHint': True, 'destructiveHint': False, 'openWorldHint': False},
            } for name, desc in [
                ('probe_inspect', 'Return the nonce for this isolated integration test.'),
                ('probe_wait', 'Wait 120 seconds for cancellation testing. No external actions.'),
            ]]}
        elif method == 'tools/call':
            name = req['params']['name']
            if name not in ('probe_inspect', 'probe_wait'):
                response['error'] = {'code': -32602, 'message': 'Unknown tool'}
                print(json.dumps(response), flush=True)
                continue
            with (args.evidence_dir / 'calls.jsonl').open('a') as f:
                f.write(json.dumps({'tool': name, 'time': time.time(), 'pid': os.getpid()}) + '\n')
            if name == 'probe_wait':
                (args.evidence_dir / 'wait.started').write_text(str(time.time()))
                time.sleep(120)
            result = {'content': [{'type': 'text', 'text': json.dumps({'probe_nonce': args.nonce})}], 'isError': False}
        else:
            response['error'] = {'code': -32601, 'message': 'Method not found'}
            print(json.dumps(response), flush=True)
            continue
        response['result'] = result
        print(json.dumps(response), flush=True)


if __name__ == '__main__':
    main()

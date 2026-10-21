import http.server, os, sys
# Serves the repo root (so record-orb.html can import app/src/orb.js) and saves posted frames.
root = os.path.abspath(os.path.join(os.path.dirname(__file__), '..', '..'))
out = os.path.dirname(os.path.abspath(__file__))
class H(http.server.SimpleHTTPRequestHandler):
    def __init__(s, *a, **k): super().__init__(*a, directory=root, **k)
    def log_message(s, *a): pass
    def do_POST(s):
        name = os.path.basename(s.path)
        data = s.rfile.read(int(s.headers['Content-Length']))
        os.makedirs(os.path.join(out, 'frames2'), exist_ok=True); open(os.path.join(out, 'frames2', name + '.png'), 'wb').write(data)
        s.send_response(204); s.end_headers()
http.server.ThreadingHTTPServer(('127.0.0.1', 8733), H).serve_forever()

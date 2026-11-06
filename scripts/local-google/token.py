"""Stands in for Google's OAuth token endpoint in the local end-to-end run (scripts/test-local-play.sh).

Every POST with the JWT-bearer grant type appends its assertion to the log file and answers a throwaway access token,
so the run can check the RS256 signature mako made with the runtime's SubtleCrypto. Anything else answers 400
invalid_grant, as Google does. Usage: python3 -I token.py PORT LOG_FILE
"""

import http.server
import json
import sys
import urllib.parse

GRANT_TYPE = "urn:ietf:params:oauth:grant-type:jwt-bearer"


class TokenEndpoint(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        form = urllib.parse.parse_qs(self.rfile.read(length).decode())
        assertion = form.get("assertion", [""])[0]
        if form.get("grant_type", [""])[0] != GRANT_TYPE or not assertion:
            self.answer(400, {"error": "invalid_grant"})
            return
        with open(sys.argv[2], "a") as log:
            log.write(assertion + "\n")
        self.answer(200, {"access_token": "local-play-token", "expires_in": 3599, "token_type": "Bearer"})

    def answer(self, status, body):
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass


http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), TokenEndpoint).serve_forever()

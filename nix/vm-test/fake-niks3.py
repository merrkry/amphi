"""Stands in for `niks3 push --stdin`: confirms every path at once, and
records it in ~/pushed for the test to inspect."""

import json
import sys
from pathlib import Path

assert sys.argv[1:3] == ["push", "--stdin"], sys.argv
with (Path.home() / "pushed").open("a") as pushed:
    for line in sys.stdin:
        request = json.loads(line)
        for path in request["paths"]:
            pushed.write(path + "\n")
            pushed.flush()
            result = {"id": request["id"], "path": path, "status": "ok"}
            print(json.dumps(result), flush=True)

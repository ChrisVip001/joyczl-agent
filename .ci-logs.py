import subprocess, json, urllib.request

r = subprocess.run(["git", "credential", "fill"],
                   input="protocol=https\nhost=github.com\n\n", capture_output=True, text=True)
token = [l.split("=", 1)[1] for l in r.stdout.splitlines() if l.startswith("password=")][0]
H = {"Authorization": f"Bearer {token}", "Accept": "application/vnd.github+json",
     "User-Agent": "joy-setup"}

def get(url):
    return json.load(urllib.request.urlopen(urllib.request.Request(url, headers=H), timeout=60))

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a, **k):
        return None

def get_raw(url):
    opener = urllib.request.build_opener(NoRedirect)
    try:
        resp = opener.open(urllib.request.Request(url, headers=H), timeout=60)
    except urllib.error.HTTPError as e:
        loc = e.headers.get("Location")
        if not loc:
            raise
        return urllib.request.urlopen(urllib.request.Request(loc, headers={"User-Agent": "joy-setup"}), timeout=120).read().decode()
    return resp.read().decode()

run = get("https://api.github.com/repos/ChrisVip001/joyczl-agent/actions/runs?per_page=1")["workflow_runs"][0]
print("run:", run["name"], run["status"], run["conclusion"])
jobs = get(run["jobs_url"])
for j in jobs["jobs"]:
    if j["conclusion"] != "failure":
        continue
    print("=" * 25, j["name"])
    log = get_raw(f"https://api.github.com/repos/ChrisVip001/joyczl-agent/actions/jobs/{j['id']}/logs")
    lines = log.split("\n")
    hits = [i for i, l in enumerate(lines) if "not ok " in l or ("strictly equal" in l)]
    seen = set()
    shown = 0
    for i in hits:
        key = lines[i][:60]
        if key in seen:
            continue
        seen.add(key)
        print("\n".join(lines[i:i+5]))
        print("---")
        shown += 1
        if shown >= 4:
            break

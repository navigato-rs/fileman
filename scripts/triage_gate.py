#!/usr/bin/env python3
"""Read-only, bounded gate for org triage. Emits references, never issue text."""
import datetime
import json
import os
import re
import sys
import urllib.parse
import urllib.request

REPOS = ("fileman", "starcom", "sunset")
HOST = "navigato-rs/fileman"
WORKFLOW = "org-triage.lock.yml"
PAGE_LIMIT = 20
BATCH_LIMIT = 30


def human(author):
    return bool(author) and author.get("type", author.get("__typename")) != "Bot" and not author.get("login", "").endswith("[bot]")


def since(value, cutoff):
    if not value:
        return False
    return datetime.datetime.fromisoformat(value.replace("Z", "+00:00")) >= datetime.datetime.fromisoformat(cutoff.replace("Z", "+00:00"))


class GitHub:
    def __init__(self, token):
        self.token = token

    def get(self, route, query=None):
        return self.request(route + ("?" + urllib.parse.urlencode(query) if query else ""))

    def request(self, route, payload=None):
        # Call sites supply fixed routes or validated integer IDs, not server links.
        request = urllib.request.Request(
            "https://api.github.com/" + route,
            data=json.dumps(payload).encode() if payload is not None else None,
            headers={"Authorization": "Bearer " + self.token, "Accept": "application/vnd.github+json", "Content-Type": "application/json", "X-GitHub-Api-Version": "2022-11-28"},
        )
        with urllib.request.urlopen(request, timeout=20) as response:
            raw = response.read(4 * 1024 * 1024 + 1)
        if len(raw) > 4 * 1024 * 1024:
            raise RuntimeError("GitHub response exceeded limit")
        result = json.loads(raw)
        if isinstance(result, dict) and result.get("errors"):
            raise RuntimeError("GitHub GraphQL read failed")
        return result

    def cutoff(self, current_run):
        for page in range(1, PAGE_LIMIT + 1):
            data = self.get(f"repos/{HOST}/actions/workflows/{WORKFLOW}/runs", {"status": "success", "per_page": 100, "page": page})
            runs = data["workflow_runs"]
            for run in runs:
                if run["id"] < current_run and run["head_branch"] == "main" and run["event"] in ("schedule", "workflow_dispatch"):
                    jobs = self.get(f"repos/{HOST}/actions/runs/{run['id']}/jobs", {"per_page": 100})
                    if jobs["total_count"] > 100:
                        raise RuntimeError("Unexpected triage job pagination")
                    # Disabled/no-work runs do not advance the triage watermark.
                    if any(job["name"] == "agent" and job["conclusion"] == "success" for job in jobs["jobs"]):
                        # Start, not finish: updates arriving during a run survive.
                        return run["created_at"]
            if len(runs) < 100:
                return "1970-01-01T00:00:00Z"
        raise RuntimeError("Workflow history exceeded limit; watermark not advanced")

    def open_issues(self, repo):
        cursor = None
        result = []
        for _ in range(PAGE_LIMIT):
            payload = self.request("graphql", {"query": """query($repo:String!, $cursor:String) {
              repository(owner:"navigato-rs", name:$repo) {
                issues(first:100, after:$cursor, states:OPEN) {
                  nodes { number createdAt lastEditedAt author { __typename login } }
                  pageInfo { hasNextPage endCursor }
                }
              }
            }""", "variables": {"repo": repo, "cursor": cursor}})
            connection = payload["data"]["repository"]["issues"]
            result.extend(connection["nodes"])
            if not connection["pageInfo"]["hasNextPage"]:
                return result
            cursor = connection["pageInfo"]["endCursor"]
        raise RuntimeError("Issue pagination exceeded limit; watermark not advanced")

    def comments(self, repo, cutoff):
        result = []
        for page in range(1, PAGE_LIMIT + 1):
            data = self.get(f"repos/navigato-rs/{repo}/issues/comments", {"since": cutoff, "sort": "updated", "direction": "desc", "per_page": 100, "page": page})
            result.extend(data)
            if len(data) < 100:
                return result
        raise RuntimeError("Comment pagination exceeded limit; watermark not advanced")


def candidates(repo, issues, comments, cutoff, summary):
    numbers = {issue["number"] for issue in issues if (repo, issue["number"]) != ("fileman", summary)}
    chosen = {issue["number"] for issue in issues if issue["number"] in numbers and human(issue["author"]) and (since(issue["createdAt"], cutoff) or since(issue["lastEditedAt"], cutoff))}
    for comment in comments:
        match = re.fullmatch(rf"https://api\.github\.com/repos/navigato-rs/{repo}/issues/([1-9][0-9]*)", comment["issue_url"])
        if match and int(match[1]) in numbers and human(comment["user"]) and since(comment["updated_at"], cutoff):
            chosen.add(int(match[1]))
    return [f"navigato-rs/{repo}#{number}" for number in sorted(chosen)]


def gate(api, current_run, summary):
    item = api.get(f"repos/{HOST}/issues/{summary}")
    if "pull_request" in item or item["state"] != "open" or item["title"] != "[triage] Navigato issue review":
        raise RuntimeError("Summary target is not the designated open triage issue")
    cutoff = api.cutoff(current_run)
    refs = []
    for repo in REPOS:
        refs += candidates(repo, api.open_issues(repo), api.comments(repo, cutoff), cutoff, summary)
    if len(refs) > BATCH_LIMIT:
        raise RuntimeError("More than 30 changed issues; review backlog before enabling triage")
    return {"has_work": str(bool(refs)).lower(), "refs": " ".join(refs), "summary": str(summary)}


def main():
    if os.environ.get("TRIAGE_ENABLED") != "true":
        result = {"has_work": "false", "refs": "", "summary": ""}
    else:
        summary = os.environ.get("TRIAGE_SUMMARY", "")
        if not re.fullmatch(r"[1-9][0-9]{0,8}", summary):
            raise RuntimeError("Configure TRIAGE_SUMMARY as the designated issue number")
        result = gate(GitHub(os.environ["GH_TOKEN"]), int(os.environ["GITHUB_RUN_ID"]), int(summary))
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        for name, value in result.items():
            print(f"{name}={value}", file=output)
    print("Triage candidates:", result["refs"] or "none (no model needed)")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Never echo an API payload, token, title, or issue text to an Actions command.
        print(f"Triage gate failed ({type(error).__name__}); no model work authorized", file=sys.stderr)
        sys.exit(1)

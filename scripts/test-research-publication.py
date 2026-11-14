"""Exercise the evaluator's publication gate without network or model access."""
import io
import json
import pathlib
import sys
import tempfile
import types


def main():
    repository = pathlib.Path(__file__).resolve().parent.parent
    source = (
        repository
        / "joy-rs/joyczl-eval/examples/live_reliability/research_case.rs"
    ).read_text().split('const SERVER: &str = r#"', 1)[1].split('"#;', 1)[0]
    with tempfile.TemporaryDirectory() as folder:
        root = pathlib.Path(folder)
        manifest = [
            {"id": "web-ui", "url": "https://example.test/web", "quote": "输入框不可用"},
            {"id": "python-sdk", "url": "https://example.test/sdk", "quote": "danger-full-access"},
        ]
        (root / "manifest.json").write_text(json.dumps(manifest))
        (root / "proof").mkdir()
        sys.argv = [
            "server", str(root / "manifest.json"), str(root / "proof"),
            str(root / "research.json"), "true",
        ]
        sys.stdin = io.StringIO("")
        scope = {}
        exec(source, scope)
        artifact = {
            "sources": manifest,
            "facts": {
                "webNeedsWorkspaceSelection": True,
                "sdkMinimalFilesystemRestricted": False,
            },
            "summary": "Workspace selection differs from filesystem isolation.",
        }

        def rejects(value):
            try:
                scope["publish"](value)
            except ValueError:
                return
            raise AssertionError("Invalid publication was accepted.")

        rejects(artifact)
        assert not (root / "research.json").exists()
        scope["subprocess"].run = lambda *args, **kwargs: types.SimpleNamespace(
            stdout="<p>输入框不可用 danger-full-access</p>".encode()
        )
        scope["read"](0)
        rejects(artifact)
        scope["read"](1)
        wrong = json.loads(json.dumps(artifact))
        wrong["facts"]["sdkMinimalFilesystemRestricted"] = True
        rejects(wrong)
        wrong = json.loads(json.dumps(artifact))
        wrong["facts"]["webNeedsWorkspaceSelection"] = 1
        rejects(wrong)
        scope["publish"](artifact)
        assert json.loads((root / "research.json").read_text()) == artifact
        assert scope["publish"](artifact)["alreadyPublished"]
        wrong = json.loads(json.dumps(artifact))
        wrong["summary"] = "Different summary."
        rejects(wrong)
        print("Publication gate rejected missing evidence and invalid facts; valid/idempotent output passed.")


if __name__ == "__main__":
    main()

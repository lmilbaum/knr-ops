import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("verify-sbom.py")


class VerifySbomTest(unittest.TestCase):
    def run_verifier(self, documents: dict[str, object]) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for name, document in documents.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(json.dumps(document))
            return subprocess.run(
                [sys.executable, str(SCRIPT), str(root)],
                check=False,
                capture_output=True,
                text=True,
            )

    def test_accepts_syft_json_documents(self):
        result = self.run_verifier(
            {
                "component.json": {
                    "artifacts": [{"name": "example"}],
                    "descriptor": {"name": "zarf", "version": "v0.83.0"},
                    "schema": {
                        "version": "16.0.0",
                        "url": "https://raw.githubusercontent.com/anchore/syft/main/schema/json/schema-16.0.0.json",
                    },
                    "source": {"type": "image"},
                }
            }
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("validated 1 Syft JSON SBOM document", result.stdout)

    def test_rejects_missing_documents(self):
        result = self.run_verifier({})

        self.assertEqual(result.returncode, 1)
        self.assertIn("no Syft JSON SBOMs", result.stderr)

    def test_rejects_non_syft_schema(self):
        result = self.run_verifier(
            {
                "component.json": {
                    "artifacts": [],
                    "descriptor": {"name": "another-tool"},
                    "schema": {"version": "1", "url": "https://example.com/schema.json"},
                    "source": {},
                }
            }
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn("schema is not an Anchore Syft JSON schema", result.stderr)


if __name__ == "__main__":
    unittest.main()

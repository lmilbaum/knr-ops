#!/usr/bin/env python3
"""Offline unit test of the action group's boundaries using Renovate's engine."""

import unittest

from renovate_harness import apply_package_rules


class ActionsGroupingTest(unittest.TestCase):
    def test_only_actions_join_group(self):
        cases = [
            ("actions/checkout", "action", "github-tags", "github-actions", True),
            ("astral-sh/setup-uv", "action", "github-tags", "github-actions", True),
            ("actions/checkout", "action", "github-digest", "github-actions", True),
            ("registry", "service", "docker", "github-actions", False),
            ("node", "container", "docker", "github-actions", False),
            ("ubuntu", "github-runner", "github-runners", "github-actions", False),
            ("node", "uses-with", "node-version", "github-actions", False),
            ("astral-sh/uv", "uses-with", "github-releases", "github-actions", False),
            ("jdx/mise", "uses-with", "github-releases", "github-actions", False),
            ("example/workflows", "workflow", "github-tags", "github-actions", False),
            ("actions/checkout", "action", "github-tags", "custom.regex", False),
        ]
        dependencies = [
            {
                "depName": name, "packageName": name, "depType": dep_type,
                "datasource": datasource, "manager": manager,
                "packageFile": ".github/workflows/test.yml",
            }
            for name, dep_type, datasource, manager, _ in cases
        ]
        results = apply_package_rules(dependencies)
        self.assertEqual(len(results), len(cases))
        for case, result in zip(cases, results):
            with self.subTest(dependency=case[:4]):
                self.assertEqual(result["groupName"] == "github-actions", case[4])


if __name__ == "__main__":
    unittest.main()

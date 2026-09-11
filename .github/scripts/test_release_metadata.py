import json
import unittest

from release_metadata import should_publish, validate_version


class ReleaseMetadataTests(unittest.TestCase):
    def setUp(self):
        self.manifest = {"package": {"name": "nasty-top", "version": "0.0.11"}}
        self.lockfile = {"package": [{"name": "nasty-top", "version": "0.0.11"}]}

    def test_tag_manifest_and_lock_must_agree(self):
        self.assertEqual(validate_version("v0.0.11", self.manifest, self.lockfile), "0.0.11")
        for tag in ["v0.0.10", "0.0.11", "master", ""]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                validate_version(tag, self.manifest, self.lockfile)
        self.lockfile["package"][0]["version"] = "0.0.10"
        with self.assertRaises(ValueError):
            validate_version("v0.0.11", self.manifest, self.lockfile)

    def test_wrong_package_name_is_rejected(self):
        self.manifest["package"]["name"] = "another-crate"
        with self.assertRaises(ValueError):
            validate_version("v0.0.11", self.manifest, self.lockfile)

    def test_existing_version_is_skipped_even_if_yanked(self):
        for yanked in [False, True]:
            body = json.dumps({"version": {"crate": "nasty-top", "num": "0.0.11", "yanked": yanked}})
            self.assertFalse(should_publish(200, body, "0.0.11"))

    def test_only_not_found_means_unpublished(self):
        self.assertTrue(should_publish(404, "not found", "0.0.12"))
        for status in [401, 403, 429, 500, 503]:
            with self.subTest(status=status), self.assertRaises(ValueError):
                should_publish(status, "error", "0.0.12")

    def test_success_response_must_match_expected_version(self):
        for body in [
            "not JSON",
            json.dumps({"version": {"crate": "other", "num": "0.0.11"}}),
            json.dumps({"version": {"crate": "nasty-top", "num": "0.0.10"}}),
        ]:
            with self.subTest(body=body), self.assertRaises(ValueError):
                should_publish(200, body, "0.0.11")


if __name__ == "__main__":
    unittest.main()

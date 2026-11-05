"""Android backup policy must preserve worlds without copying private credentials."""
from pathlib import Path
import unittest
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[2]
ANDROID = ROOT / "packaging/android"


class AndroidDataBackupTests(unittest.TestCase):
    def test_manifest_enables_only_the_scoped_world_backup_rules(self):
        manifest = ET.parse(ANDROID / "AndroidManifest.xml").getroot()
        namespace = "{http://schemas.android.com/apk/res/android}"
        application = manifest.find("application")
        self.assertIsNotNone(application)
        self.assertEqual(application.attrib[namespace + "allowBackup"], "true")
        self.assertEqual(application.attrib[namespace + "fullBackupContent"], "@xml/backup_rules")
        self.assertEqual(application.attrib[namespace + "dataExtractionRules"],
                         "@xml/data_extraction_rules")

    def test_legacy_backup_contains_only_local_world_data(self):
        rules = ET.parse(ANDROID / "res/xml/backup_rules.xml").getroot()
        includes = [node.attrib for node in rules.findall("include")]
        self.assertEqual(includes, [{"domain": "file", "path": "data/worlds/"}])
        self.assertEqual(rules.findall("exclude"), [])

    def test_android_12_cloud_and_device_transfer_contain_only_local_world_data(self):
        rules = ET.parse(ANDROID / "res/xml/data_extraction_rules.xml").getroot()
        expected = [{"domain": "file", "path": "data/worlds/"}]
        for section_name in ("cloud-backup", "device-transfer"):
            with self.subTest(section=section_name):
                section = rules.find(section_name)
                self.assertIsNotNone(section)
                self.assertEqual([node.attrib for node in section.findall("include")], expected)
                self.assertEqual(section.findall("exclude"), [])


if __name__ == "__main__":
    unittest.main()

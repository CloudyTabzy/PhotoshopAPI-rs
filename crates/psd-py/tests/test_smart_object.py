"""Smart Object source access and replacement through live layer handles."""

import unittest
from pathlib import Path

import numpy as np

import photoshopapi as psapi


FIXTURES = Path(__file__).resolve().parents[3] / "fixtures" / "documents" / "SmartObjects"
LAYER_PATH = "simple_warp/bbox_change_perspective_warp"


class SmartObjectBindingsTest(unittest.TestCase):
    def test_warp_points_are_editable_and_applied_to_layer(self):
        document = psapi.LayeredFile.read(FIXTURES / "smart_objects_transformed.psd")
        layer = document.find_layer(LAYER_PATH)
        warp = layer.warp
        self.assertIsInstance(warp, psapi.SmartObjectWarp)
        self.assertGreaterEqual(warp.u_dims, 4)
        self.assertGreaterEqual(warp.v_dims, 4)
        points = warp.points
        self.assertEqual(len(points), warp.u_dims * warp.v_dims)
        original = points[0]
        points[0] = original + 5.0
        warp.points = points
        layer.warp = warp
        self.assertAlmostEqual(layer.warp.points[0].x, original.x + 5.0)

        generated = psapi.SmartObjectWarp.generate_default(200, 108)
        self.assertEqual(len(generated.points), 16)

    def test_embedded_source_and_replacement_roundtrip(self):
        document = psapi.LayeredFile.read(FIXTURES / "smart_objects_transformed.psd")
        layer = document.find_layer(LAYER_PATH)
        self.assertIsInstance(layer, psapi.SmartObjectLayer_8bit)
        self.assertTrue(layer.hash())
        self.assertTrue(layer.filename())
        self.assertEqual(layer.linkage, psapi.enum.LinkedLayerType.data)

        original = layer.get_original_image_data()
        self.assertEqual((layer.original_width(), layer.original_height()), (512, 512))
        self.assertEqual(original[0].shape, (512, 512))
        self.assertEqual(original[0].dtype, np.uint8)

        previous_identity = layer.hash()
        replacement = FIXTURES / "reference" / "control.png"
        layer.replace(replacement)
        self.assertEqual(layer.filename(), "control.png")
        self.assertNotEqual(layer.hash(), previous_identity)
        self.assertGreater(layer.original_width(), 0)

        reread = psapi.LayeredFile_8bit.from_bytes(document.to_bytes())
        replaced = reread.find_layer(LAYER_PATH)
        self.assertEqual(replaced.filename(), "control.png")
        self.assertEqual(replaced.linkage, psapi.enum.LinkedLayerType.data)

    def test_external_replacement_records_source_path(self):
        document = psapi.LayeredFile.read(FIXTURES / "smart_objects_transformed.psd")
        layer = document.find_layer(LAYER_PATH)
        replacement = FIXTURES / "reference" / "control.png"
        layer.replace(replacement, link_externally=True)
        self.assertEqual(layer.linkage, psapi.enum.LinkedLayerType.external)
        self.assertEqual(layer.filepath(), replacement.resolve())


if __name__ == "__main__":
    unittest.main()

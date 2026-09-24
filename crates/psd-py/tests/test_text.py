"""Text-layer Python API against the upstream PSD fixtures."""

import unittest
from pathlib import Path

import photoshopapi as psapi


FIXTURES = Path(__file__).resolve().parents[3] / "fixtures" / "documents" / "TextLayers"


class TextBindingsTest(unittest.TestCase):
    def test_unicode_edit_font_and_box_roundtrip(self):
        document = psapi.LayeredFile.read(FIXTURES / "TextLayers_Basic.psd")
        layer = next(
            layer
            for layer in document.flat_layers
            if isinstance(layer, psapi.TextLayer_8bit) and layer.text == "Hello 123"
        )
        self.assertIsInstance(layer, psapi.TextLayer_8bit)
        layer.set_text("你好 😀")
        self.assertEqual(layer.text, "你好 😀")
        font_index = layer.add_font("PhaseFourTestFont")
        self.assertEqual(layer.font_postscript_name(font_index), "PhaseFourTestFont")
        self.assertEqual(layer.font_type(font_index), psapi.enum.FontType.OpenType)
        self.assertEqual(layer.font_script(font_index), psapi.enum.FontScript.Roman)
        self.assertEqual(layer.find_font_index("PhaseFourTestFont"), font_index)

        reread = psapi.LayeredFile_8bit.from_bytes(document.to_bytes())
        edited = next(
            layer
            for layer in reread.flat_layers
            if isinstance(layer, psapi.TextLayer_8bit) and layer.text == "你好 😀"
        )
        self.assertEqual(edited.font_postscript_name(font_index), "PhaseFourTestFont")

        created = psapi.TextLayer_8bit(
            "Box", "Hello", box_width=120.0, box_height=40.0
        )
        self.assertTrue(created.is_box_text)
        self.assertEqual(created.shape_type(), psapi.enum.ShapeType.BoxText)
        created.set_box_size(90.0, 30.0)
        self.assertAlmostEqual(created.box_width(), 90.0)
        self.assertAlmostEqual(created.box_height(), 30.0)

    def test_warp_and_transform_mutations_roundtrip(self):
        document = psapi.LayeredFile.read(FIXTURES / "TextLayers_Warp.psd")
        layer = document.find_layer("WarpArc")
        self.assertIsInstance(layer, psapi.TextLayer_8bit)
        self.assertTrue(layer.has_warp)
        self.assertEqual(layer.warp_style, psapi.enum.WarpStyle.Arc)
        self.assertAlmostEqual(layer.warp_value, 28.0)
        layer.set_warp_style(psapi.enum.WarpStyle.Wave)
        layer.set_warp_value(-22.5)
        layer.set_warp_rotation(psapi.enum.WarpRotation.Vertical)
        self.assertEqual(layer.warp_style, psapi.enum.WarpStyle.Wave)
        self.assertEqual(layer.warp_rotation, psapi.enum.WarpRotation.Vertical)

        self.assertEqual(len(layer.transform()), 6)
        layer.set_position(17.0, 23.0)
        self.assertEqual(layer.position(), (17.0, 23.0))
        layer.set_scale(1.5)
        self.assertAlmostEqual(layer.scale_x, 1.5)

        reread = psapi.LayeredFile_8bit.from_bytes(document.to_bytes())
        edited = reread.find_layer("WarpArc")
        self.assertEqual(edited.warp_style, psapi.enum.WarpStyle.Wave)
        self.assertAlmostEqual(edited.warp_value, -22.5)
        self.assertEqual(edited.warp_rotation, psapi.enum.WarpRotation.Vertical)
        self.assertEqual(edited.position(), (17.0, 23.0))


if __name__ == "__main__":
    unittest.main()

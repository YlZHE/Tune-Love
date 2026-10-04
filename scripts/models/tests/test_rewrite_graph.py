"""Tests for rewrite_graph: small onnx graphs run on onnxruntime CPU, original vs rewritten."""
import sys
import unittest
from pathlib import Path

import numpy as np
import onnx
import onnxruntime as ort
from onnx import TensorProto, helper, numpy_helper

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import rewrite_graph as rg  # noqa: E402


ort.set_default_logger_severity(3)


def make_model(nodes, inputs, outputs, inits=()):
    g = helper.make_graph(nodes, "t", inputs, outputs, list(inits))
    m = helper.make_model(g, opset_imports=[helper.make_opsetid("", 17)])
    m.ir_version = 8
    onnx.checker.check_model(m)
    return m


def run(m, x):
    s = ort.InferenceSession(m.SerializeToString(), providers=["CPUExecutionProvider"])
    return s.run(None, {s.get_inputs()[0].name: x})


def vi(name, shape):
    return helper.make_tensor_value_info(name, TensorProto.FLOAT, shape)


class RewriteGraphTest(unittest.TestCase):
    def test_convtranspose_rewrite_matches(self):
        rng = np.random.RandomState(0)
        w = rng.randn(8, 1, 256).astype(np.float32)
        node = helper.make_node("ConvTranspose", ["x", "w"], ["y"], strides=[64], kernel_shape=[256])
        m = make_model([node], [vi("x", [1, 8, 20])], [vi("y", [1, 1, None])], [numpy_helper.from_array(w, "w")])
        x = rng.randn(1, 8, 20).astype(np.float32)
        before = run(m, x)[0]
        m2 = rg.rewrite_convtranspose(m)
        self.assertNotIn("ConvTranspose", [n.op_type for n in m2.graph.node])
        after = run(m2, x)[0]
        self.assertEqual(before.shape, after.shape)
        self.assertLessEqual(float(np.abs(before - after).max()), 1e-6)

    def test_convtranspose_rewrite_with_bias_and_non_multiple_kernel(self):
        rng = np.random.RandomState(1)
        w = rng.randn(4, 2, 300).astype(np.float32)  # K=300 is not a multiple of S=64
        b = rng.randn(2).astype(np.float32)
        node = helper.make_node("ConvTranspose", ["x", "w", "b"], ["y"], strides=[64], kernel_shape=[300])
        m = make_model([node], [vi("x", [1, 4, 9])], [vi("y", [1, 2, None])],
                       [numpy_helper.from_array(w, "w"), numpy_helper.from_array(b, "b")])
        x = rng.randn(1, 4, 9).astype(np.float32)
        before = run(m, x)[0]
        after = run(rg.rewrite_convtranspose(m), x)[0]
        self.assertEqual(before.shape, after.shape)
        self.assertLessEqual(float(np.abs(before - after).max()), 1e-5)

    def test_convtranspose_outside_conditions_is_kept(self):
        w = np.random.RandomState(2).randn(2, 1, 16).astype(np.float32)  # kernel < 256
        node = helper.make_node("ConvTranspose", ["x", "w"], ["y"], strides=[4], kernel_shape=[16])
        m = make_model([node], [vi("x", [1, 2, 5])], [vi("y", [1, 1, None])], [numpy_helper.from_array(w, "w")])
        self.assertEqual([n.op_type for n in rg.rewrite_convtranspose(m).graph.node], ["ConvTranspose"])

    def test_split_to_slice_matches(self):
        node = helper.make_node("Split", ["x"], ["a", "b"], axis=1)
        m = make_model([node], [vi("x", [1, 6, 5])], [vi("a", [1, 3, 5]), vi("b", [1, 3, 5])])
        x = np.random.RandomState(3).randn(1, 6, 5).astype(np.float32)
        before = run(m, x)
        m2 = rg.split_to_slice(m)
        self.assertNotIn("Split", [n.op_type for n in m2.graph.node])
        after = run(m2, x)
        for a, b in zip(before, after):
            np.testing.assert_array_equal(a, b)

    def test_split_to_slice_uses_inferred_shape_of_intermediate(self):
        nodes = [helper.make_node("Relu", ["x"], ["r"]), helper.make_node("Split", ["r"], ["a", "b"], axis=2)]
        m = make_model(nodes, [vi("x", [1, 3, 8])], [vi("a", [1, 3, 4]), vi("b", [1, 3, 4])])
        x = np.random.RandomState(4).randn(1, 3, 8).astype(np.float32)
        before = run(m, x)
        m2 = rg.split_to_slice(m)
        self.assertNotIn("Split", [n.op_type for n in m2.graph.node])
        for a, b in zip(before, run(m2, x)):
            np.testing.assert_array_equal(a, b)

    def test_drop_unused_initializers(self):
        keep = numpy_helper.from_array(np.ones((3,), np.float32), "keep")
        junk = numpy_helper.from_array(np.zeros((1000,), np.float32), "junk")
        node = helper.make_node("Add", ["x", "keep"], ["y"])
        m = make_model([node], [vi("x", [3])], [vi("y", [3])], [keep, junk])
        m2 = rg.drop_unused_initializers(m)
        self.assertEqual([i.name for i in m2.graph.initializer], ["keep"])
        np.testing.assert_array_equal(run(m2, np.arange(3, dtype=np.float32))[0], np.arange(3, dtype=np.float32) + 1)


if __name__ == "__main__":
    unittest.main()

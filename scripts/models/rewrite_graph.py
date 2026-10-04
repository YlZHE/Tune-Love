"""ONNX graph rewrites that make the separation models run fast and correctly on DirectML.

All functions edit the model in place and return it (a 300 MB model is not worth copying).

- rewrite_convtranspose: a large-kernel ConvTranspose (the iSTFT overlap-add) is extremely slow on
  DirectML. It becomes a MatMul plus a Pad/Sum overlap-add, which matches the original on CPU.
- split_to_slice: DirectML graph fusion computes the GLU `Split` wrongly (HTDemucs came out at
  about -106 dB against CPU). Equal splits become Slice nodes, which fixes it.
- drop_unused_initializers: the rewrite leaves the old weights behind; remove them.

See work/directml-spike-20261004/report.md section 2.
"""
import numpy as np
from onnx import helper, numpy_helper, shape_inference

MIN_KERNEL = 256  # smaller kernels run fine on DirectML and are left alone


def _constants(graph):
    """name -> numpy value for initializers and Constant nodes."""
    out = {i.name: numpy_helper.to_array(i) for i in graph.initializer}
    for n in graph.node:
        if n.op_type == "Constant":
            for a in n.attribute:
                if a.name == "value":
                    out[n.output[0]] = numpy_helper.to_array(a.t)
    return out


def _attr(node, name, default=None):
    for a in node.attribute:
        if a.name == name:
            return helper.get_attribute_value(a)
    return default


class _Emitter:
    """Collects new nodes and the int64 / weight initializers they need, with unique names."""

    def __init__(self, tag):
        self.tag, self.nodes, self.inits, self._n = tag, [], [], 0

    def name(self, base):
        self._n += 1
        return f"{base}__{self.tag}{self._n}"

    def ints(self, values, base="c"):
        t = numpy_helper.from_array(np.asarray(values, dtype=np.int64), self.name(base))
        self.inits.append(t)
        return t.name

    def array(self, arr, base):
        t = numpy_helper.from_array(arr, self.name(base))
        self.inits.append(t)
        return t.name

    def node(self, op, inputs, base, **attrs):
        out = self.name(base)
        self.nodes.append(helper.make_node(op, inputs, [out], **attrs))
        return out

    def slice(self, x, start, end, axis):
        return self.node("Slice", [x, self.ints([start]), self.ints([end]), self.ints([axis])], "sl")


def _finish(model, em):
    del model.graph.node[:]
    model.graph.node.extend(em.nodes)
    model.graph.initializer.extend(em.inits)
    return model


def _convtranspose_ok(node, consts, min_kernel):
    """Constant weight (and bias, if any), group 1, no dilation, auto_pad NOTSET, no pads /
    output_padding / output_shape, stride > 1, 1-D or (K, 1) 2-D, kernel >= min_kernel."""
    w = consts.get(node.input[1]) if len(node.input) > 1 else None
    if w is None or w.ndim not in (3, 4):
        return False
    if len(node.input) > 2 and node.input[2] and node.input[2] not in consts:
        return False
    two_d = w.ndim == 4
    k = list(_attr(node, "kernel_shape") or w.shape[2:])
    strides = list(_attr(node, "strides") or [1] * (w.ndim - 2))
    return (
        strides[0] > 1
        and _attr(node, "auto_pad", b"NOTSET") in (b"NOTSET", "NOTSET")
        and _attr(node, "group", 1) == 1
        and all(d == 1 for d in (_attr(node, "dilations") or [1]))
        and not any(_attr(node, "pads") or [])
        and not _attr(node, "output_padding")
        and not _attr(node, "output_shape")
        and k[0] >= min_kernel
        and (not two_d or (k[1] == 1 and strides[1] == 1))
    )


def _overlap_add(em, x, w, bias, stride):
    """x: (B, Cin, T); w: (Cin, Cout, K) -> (B, Cout, (T-1)*stride + K), same as ConvTranspose."""
    cin, cout, k = w.shape
    parts_n = -(-k // stride)  # ceil(K / S)
    kp = parts_n * stride
    wp = np.zeros((cin, cout, kp), dtype=w.dtype)
    wp[:, :, :k] = w
    xt = em.node("Transpose", [x], "xt", perm=[0, 2, 1])  # (B, T, Cin)
    z = em.node("MatMul", [xt, em.array(wp.reshape(cin, cout * kp), "W")], "z")  # (B, T, Cout*Kp)
    z5 = em.node("Reshape", [z, em.ints([0, 0, cout, parts_n, stride], "shape")], "z5")
    pads = lambda j: [0, j, 0, 0, 0, 0, parts_n - 1 - j, 0, 0, 0]  # shift part j by j frames
    parts = [em.node("Pad", [em.slice(z5, j, j + 1, 3), em.ints(pads(j), "pads")], "pj") for j in range(parts_n)]
    s = em.node("Sum", parts, "sum")  # (B, T+m-1, Cout, 1, S)
    y = em.node("Transpose", [s], "tr", perm=[0, 2, 1, 3, 4])
    y = em.node("Reshape", [y, em.ints([0, 0, -1], "shape")], "y")  # (B, Cout, (T+m-1)*S)
    if kp > k:
        y = em.slice(y, 0, -(kp - k), 2)
    if bias is not None:
        y = em.node("Add", [y, em.array(bias.reshape(cout, 1), "bias")], "y")
    return y


def rewrite_convtranspose(model, min_kernel=MIN_KERNEL):
    consts = _constants(model.graph)
    em = _Emitter("ola")
    for n in model.graph.node:
        if n.op_type != "ConvTranspose" or not _convtranspose_ok(n, consts, min_kernel):
            em.nodes.append(n)
            continue
        w = consts[n.input[1]]
        two_d = w.ndim == 4
        x = n.input[0]
        if two_d:
            x = em.node("Squeeze", [x, em.ints([3])], "sq")
        bias = consts[n.input[2]] if len(n.input) > 2 and n.input[2] else None
        y = _overlap_add(em, x, w[..., 0] if two_d else w, bias, list(_attr(n, "strides"))[0])
        if two_d:
            em.nodes.append(helper.make_node("Unsqueeze", [y, em.ints([3])], [n.output[0]]))
        else:
            em.nodes.append(helper.make_node("Identity", [y], [n.output[0]]))
    return _finish(model, em)


def split_to_slice(model):
    """Replace every Split without a `split` input or legacy `split` attribute (equal parts) by equal-width Slices. The width
    comes from shape inference; a Split whose axis size is unknown or not divisible stays."""
    inferred = shape_inference.infer_shapes(model).graph
    dims = {}
    for v in list(inferred.input) + list(inferred.value_info):
        dims[v.name] = [d.dim_value if d.HasField("dim_value") else None for d in v.type.tensor_type.shape.dim]
    em = _Emitter("sl")
    for n in model.graph.node:
        if n.op_type == "Split" and len(n.input) == 1 and not any(a.name == "split" for a in n.attribute):
            axis = _attr(n, "axis", 0)
            shape = dims.get(n.input[0])
            size = shape[axis] if shape and -len(shape) <= axis < len(shape) else None
            if size and size % len(n.output) == 0:
                part = size // len(n.output)
                for j, out in enumerate(n.output):
                    em.nodes.append(helper.make_node("Slice", [
                        n.input[0], em.ints([j * part]), em.ints([(j + 1) * part]), em.ints([axis])], [out]))
                continue
        em.nodes.append(n)
    return _finish(model, em)


def _used_names(graph):
    used = {o.name for o in graph.output}
    for n in graph.node:
        used.update(n.input)
        for a in n.attribute:  # subgraphs of If / Loop / Scan
            subs = [a.g] if a.HasField("g") else list(a.graphs)
            for sub in subs:
                used |= _used_names(sub)
    return used


def drop_unused_initializers(model):
    """Remove initializers, and Constant nodes, that nothing reads."""
    used = _used_names(model.graph)
    keep = [i for i in model.graph.initializer if i.name in used]
    del model.graph.initializer[:]
    model.graph.initializer.extend(keep)
    live = [n for n in model.graph.node if n.op_type != "Constant" or n.output[0] in used]
    del model.graph.node[:]
    model.graph.node.extend(live)
    return model


def count_ops(model, op_type):
    return sum(n.op_type == op_type for n in model.graph.node)


def count_large_convtranspose(model, min_kernel=MIN_KERNEL):
    """ConvTranspose nodes whose first kernel dimension is >= min_kernel (what the rewrite targets)."""
    consts = _constants(model.graph)
    total = 0
    for n in model.graph.node:
        if n.op_type == "ConvTranspose":
            k = _attr(n, "kernel_shape") or (consts[n.input[1]].shape[2:] if n.input[1] in consts else [0])
            total += k[0] >= min_kernel
    return total

/*
 * Copyright 2018 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Test for `def` and `lambda`.

use crate::assert;
use crate::assert::Assert;
use crate::environment::Module;
use crate::eval::Evaluator;

#[test]
fn test_lambda() {
    assert::is_true("(lambda x: x)(1) == 1");
    assert::is_true("(lambda x: (x == 1))(1)");
    assert::is_true(
        "
xs = [lambda x: x + y for y in [1,2,3]]
ys = [lambda x: x + y for y in [4,5,6]]
[xs[1](0),ys[1](0)] == [3,6]",
    );
}

#[test]
fn test_frozen_lambda() {
    let mut a = Assert::new();
    a.module(
        "lam",
        r#"
def my_func(a):
    return lambda b: a + b
add18 = my_func(18)
# This test used to fail if a GC happened, so add one
garbage_collect()
"#,
    );
    a.pass(
        r#"
load("lam", "add18")
assert_eq(add18(24), 42)
"#,
    );
}

#[test]
fn test_def_default_per_instantiation() {
    assert::is_true(
        "
def outer():
    fs = []
    for i in [1, 2, 3]:
        def g(x = i):
            return x
        fs.append(g)
    return [f() for f in fs]
outer() == [1, 2, 3]",
    );
}

#[test]
fn test_def_default_per_instantiation_frozen() {
    let mut a = Assert::new();
    a.module(
        "defs",
        r#"
fs = []
for i in [1, 2, 3]:
    def g(x = i):
        return x
    fs.append(g)
garbage_collect()
"#,
    );
    a.pass(
        r#"
load("defs", "fs")
assert_eq([f() for f in fs], [1, 2, 3])
"#,
    );
}

#[test]
fn test_nested_def_1() {
    assert::is_true(
        "
def foo(x):
    def bar(y):
        return x+y
    return bar(x)
foo(8) == 16",
    );
}

#[test]
fn test_nested_def_2() {
    assert::is_true(
        "
def squarer():
    x = [0]
    def f():
        x[0] += 1
        return x[0]*x[0]
    return f
sq = squarer()
[sq(), sq(), sq(), sq()] == [1,4,9,16]",
    );
}

#[test]
fn test_nested_def_3() {
    assert::is_true(
        "
def f(x):
    def g(y):
        return lambda z: x + y + z
    return g
f(1)(2)(3) == 6",
    );
}

#[test]
fn test_lambda_capture_from_module() {
    assert::is_true(
        "
f = lambda y: x + y
x = 100
f(42) == 142
",
    );
}

#[test]
fn test_lambda_capture_from_def() {
    assert::is_true(
        "
def inside():
    f = lambda y: x + y
    x = 100
    return f(42) == 142
inside()
",
    );
}

#[test]
fn test_lambda_capture_reassigned_from_def() {
    assert::is_true(
        "
def inside():
    x = 100
    f = lambda y: x + y
    x = 200
    return f(42) == 242
inside()
",
    );
}

#[test]
fn test_def_freeze() {
    let mut a = Assert::new();
    a.module(
        "f.bzl",
        r#"
def f(g):
    g(1)"#,
    );
    a.is_true(
        r#"
load('f.bzl', 'f')
x = []
def g(y):
    x.append(y)
f(g)
x == [1]"#,
    );
}

#[test]
fn test_frozen_lambda_nest() {
    let mut a = Assert::new();
    let m = a.module(
        "a",
        r#"
def outer_function(x):
    return x["test"]

def function(x):
    def inner_function():
        return outer_function(x)
    return inner_function()

value = {"test": "hello"}
"#,
    );
    let f = m.get("function").unwrap();
    let x = m.get("value").unwrap();
    Module::with_temp_heap(|module| {
        let f = f.add_to_heap(module.heap());
        let x = x.add_to_heap(module.heap());
        let mut eval = Evaluator::new(&module);
        let res = eval.eval_function(f, &[x], &[]).unwrap();
        assert_eq!(res.to_str(), "hello");
        crate::Result::Ok(())
    })
    .unwrap();
}

#[test]
fn test_context_captured() {
    let mut a = Assert::new();
    a.module("f.bzl", "x = 17\ndef f(): return x");
    // Import `f` but do not import `x`
    a.is_true("load('f.bzl', 'f')\nf() == 17");
}

#[test]
fn test_lambda_errors() {
    // Test from https://github.com/facebook/starlark-rust/issues/36
    assert::fail("lambda a,a:a", "duplicated parameter name");
}

#[test]
fn test_lambda_errors_nested() {
    // Test from https://issues.oss-fuzz.com/issues/369003809
    assert::fail("lambda: lambda a,a:a", "duplicated parameter name");
    assert::fail("[lambda a,a:a]", "duplicated parameter name");
}

#[test]
fn test_double_capture_and_freeze() {
    let mut a = Assert::new();
    a.module(
        "x.bzl",
        r#"
def f(x):
    # `x` is captured by `g` and then frozen.
    def g():
        # When `h` is instantiated, `x` is already captured and frozen.
        def h():
            return noop(x)
        return h

    return g

G = f(1)
     "#,
    );

    a.pass("load('x.bzl', 'G')\nG()");
}

/// The self-contained source of `func` in a module `parsers.bzl` holding `code`.
fn self_contained_source(a: &mut Assert, code: &str, func: &str) -> Result<String, String> {
    let module = a.module("parsers", code);
    let func = module.get(func).unwrap();
    func.as_ref()
        .value()
        .self_contained_function_source()
        .map(|source| {
            assert_eq!(source.filename, "parsers.bzl");
            source.source.to_owned()
        })
}

const PURE_PARSERS: &str = r#"
"""Parsers that read only their arguments and the standard builtins."""

def parse_listing(stdout: str) -> list:
    if not stdout.endswith("\n"):
        fail("incomplete listing: {}".format(repr(stdout)))
    return [{"filter": name, "name": name} for name in stdout[:-1].split("\n") if name]

def parse_result(stdout, stderr, exit_code = 0):
    cases = []
    for index, line in enumerate(stdout.split("\n")):
        name, _separator, outcome = line.partition(" ... ")
        if type(outcome) == "string" and outcome.isdigit():
            cases.append((name, int(outcome), index))
    for _index in range(len(cases)):
        pass
    # A local may share a name with a builtin the host provides; it is still a local.
    print = len
    return _summary(cases, stderr, exit_code, print(stdout))

def _summary(cases, stderr, exit_code, length):
    return [{
        "cases": tuple(cases),
        "length": length,
        "message": None if exit_code == 0 else stderr,
        "status": "PASS" if exit_code == 0 else "FAIL",
    }]
"#;

#[test]
fn test_self_contained_function_source_admits_standard_builtins_and_methods() {
    let mut a = Assert::new();
    for func in ["parse_listing", "parse_result"] {
        assert_eq!(
            self_contained_source(&mut a, PURE_PARSERS, func).as_deref(),
            Ok(PURE_PARSERS)
        );
    }
}

#[test]
fn test_self_contained_function_source_rejects_host_builtin() {
    // A host global such as Buck's `read_config` can return different values for the same
    // source, so the source alone does not determine the function.
    let mut a = Assert::new();
    a.globals_add(|g| g.set("read_config", "PASS"));
    let code = r#"
def parse_listing(stdout):
    return [{"name": "case", "filter": "case"}]

def parse_result(stdout, stderr, exit_code):
    return [{"name": "case", "status": read_config}]
"#;
    for func in ["parse_listing", "parse_result"] {
        let err = self_contained_source(&mut a, code, func).unwrap_err();
        assert!(
            err.contains("neither its own nor a standard Starlark builtin")
                && err.contains("read_config"),
            "{err}"
        );
    }
}

#[test]
fn test_self_contained_function_source_rejects_non_standard_builtin_in_helper() {
    // `print` is a library extension, not a standard builtin; a helper's use counts as well.
    let mut a = Assert::new();
    let code = r#"
def parse(stdout):
    return _helper(stdout)

def _helper(stdout):
    print(stdout)
    return []
"#;
    let err = self_contained_source(&mut a, code, "parse").unwrap_err();
    assert!(err.contains("print"), "{err}");
}

#[test]
fn test_self_contained_function_source_rejects_module_value_and_load() {
    let mut a = Assert::new();
    let err = self_contained_source(
        &mut a,
        "STATUS = 'PASS'\ndef parse(x):\n    return STATUS\n",
        "parse",
    )
    .unwrap_err();
    assert!(
        err.contains("top-level statement other than `def`"),
        "{err}"
    );
    a.module("other", "def helper():\n    return 1\n");
    let err = self_contained_source(
        &mut a,
        "load('other', 'helper')\ndef parse(x):\n    return helper()\n",
        "parse",
    )
    .unwrap_err();
    assert!(
        err.contains("top-level statement other than `def`"),
        "{err}"
    );
}

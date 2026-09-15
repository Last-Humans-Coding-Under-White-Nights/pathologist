static int target1() { return 1; }
static int target2() { return 2; }
static int target3() { return 3; }
static int target4() { return 4; }
static int target5() { return 5; }
static int target6() { return 6; }
static int target7() { return 7; }
static int target8() { return 8; }
static int target9() { return 9; }
static int target10() { return 10; }
static int target11() { return 11; }
static int target12() { return 12; }

typedef int (*fn_t)();

// 1. Explicit by-value capture
static void test_explicit_val() {
    fn_t f1 = target1;
    auto l1 = [f1]() {
        f1();
    };
    l1();
}

// 2. Explicit by-reference capture
static void test_explicit_ref() {
    fn_t f2 = target2;
    auto l2 = [&f2]() {
        f2();
    };
    l2();
}

// 3. Default by-reference capture
static void test_default_ref() {
    fn_t f3 = target3;
    auto l3 = [&]() {
        f3();
    };
    l3();
}

// 4. Default by-value capture
static void test_default_val() {
    fn_t f4 = target4;
    auto l4 = [=]() {
        f4();
    };
    l4();
}

// 5. Mixed: default ref, except f1 by value
static void test_mixed_ref_val() {
    fn_t f1 = target1;
    fn_t f2 = target2;
    auto l5 = [&, f1]() {
        f1();
        f2();
    };
    l5();
}

// 6. Mixed: default val, except f2 by ref
static void test_mixed_val_ref() {
    fn_t f1 = target1;
    fn_t f2 = target2;
    auto l6 = [=, &f2]() {
        f1();
        f2();
    };
    l6();
}

// 7. Init-capture
static void test_init_capture() {
    fn_t f5 = target5;
    auto l7 = [cb = f5]() {
        cb();
    };
    l7();
}

// 8. Base & Derived classes with [this] and [*this]
class Base {
public:
    void baseAction() {
        target6();
    }
};

class Derived : public Base {
public:
    void derivedAction() {
        target7();
    }

    void testThisCapture() {
        auto l = [this]() {
            this->derivedAction();
            derivedAction();
            baseAction();
        };
        l();
    }

    void testStarThisCapture() {
        auto l = [*this]() {
            derivedAction();
        };
        l();
    }

    void testDefaultCapturesThis() {
        auto l = [&]() {
            derivedAction();
        };
        l();
    }

    void testNoCapture() {
        auto l = []() {
            target8();
        };
        l();
    }
};

static void test_class_captures() {
    Derived d;
    d.testThisCapture();
    d.testStarThisCapture();
    d.testDefaultCapturesThis();
    d.testNoCapture();
}

// 9. Parameter shadowing captured variable
static void test_param_shadow() {
    fn_t f1 = target1;
    auto l = [f1](fn_t f1) {
        f1();
    };
    l(target2);
}

// 10. Nested lambdas
static void test_nested_lambdas() {
    fn_t f1 = target1;
    auto outer = [f1]() {
        auto inner = [f1]() {
            f1();
        };
        inner();
    };
    outer();
}

// 11. Lambda stored in struct field
struct Holder {
    fn_t cb;
};

static void test_lambda_in_struct() {
    Holder h;
    fn_t f1 = target1;
    h.cb = [f1]() -> int {
        return f1();
    };
    h.cb();
}

// 12. Init-capture by reference
static void test_init_capture_ref() {
    fn_t f9 = target9;
    auto l = [&cb = f9]() {
        cb();
    };
    l();
}

// 13. Captured object calling method
struct Worker {
    void doWork() {
        target10();
    }
};

static void test_captured_object() {
    Worker w;
    auto l = [&w]() {
        w.doWork();
    };
    l();
}

// 14. Captured pointer calling method
static void test_captured_pointer() {
    Worker w;
    Worker* pw = &w;
    auto l = [pw]() {
        pw->doWork();
    };
    l();
}

// 15. Captured variable passed as argument to helper function
static void helper_call(fn_t fn) {
    fn();
}

static void test_captured_arg_pass() {
    fn_t f11 = target11;
    auto l = [f11]() {
        helper_call(f11);
    };
    l();
}

// 16. Service class lambda accessing member variables and methods
class Service {
public:
    fn_t handler;
    Worker worker;

    void process() {
        target6();
    }

    void testMemberAccess() {
        handler = target12;
        auto l1 = [this]() {
            this->process();
            process();
            worker.doWork();
            handler();
        };
        l1();

        auto l2 = [&]() {
            worker.doWork();
            handler();
        };
        l2();

        auto l3 = [=]() {
            worker.doWork();
            handler();
        };
        l3();
    }
};

static void test_service_lambda() {
    Service s;
    s.testMemberAccess();
}

// 17. Lambda returning a function pointer
static void test_lambda_returns_fn() {
    fn_t f1 = target1;
    auto l = [f1]() {
        return f1;
    };
    auto res = l();
    res();
}

// 18. Multiple captures list
static void test_multi_captures() {
    fn_t f1 = target1;
    fn_t f2 = target2;
    fn_t f3 = target3;
    fn_t f4 = target4;
    auto l = [f1, &f2, f3, &f4]() {
        f1();
        f2();
        f3();
        f4();
    };
    l();
}

// 19. Mutable lambda
static void test_mutable_lambda() {
    fn_t f1 = target1;
    auto l = [f1]() mutable {
        f1();
    };
    l();
}

// 20. 3-level nested lambdas
static void test_multi_nested_lambdas() {
    fn_t f1 = target1;
    auto outer = [f1]() {
        auto mid = [f1]() {
            auto inner = [f1]() {
                f1();
            };
            inner();
        };
        mid();
    };
    outer();
}

// 21. Lexical class lookup for captureless lambda calling static member
struct LexicalClass {
    static int hit() {
        return target1();
    }
    void run() {
        auto l = []() {
            hit();
        };
        l();
    }
};

static void test_captureless_lexical_lookup() {
    LexicalClass c;
    c.run();
}

// 22. Write through reference init-capture
static void test_ref_init_capture_write() {
    fn_t f = target1;
    auto l = [&cb = f]() {
        cb = target2;
    };
    l();
    f();
}

// 23. Repeated callable invocations preserving distinct return destinations
static void test_repeated_call_returns() {
    fn_t f = target1;
    auto l = [f]() {
        return f;
    };
    auto a = l();
    auto b = l();
    a();
    b();
}


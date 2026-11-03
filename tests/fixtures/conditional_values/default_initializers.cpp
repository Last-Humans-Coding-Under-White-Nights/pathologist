using Callback = void (*)();
extern bool choice;
void member_alpha0() {}
void member_beta0() {}
void member_alpha1() {}
void member_beta1() {}
void member_alpha2() {}
void member_beta2() {}
void member_alpha3() {}
void member_beta3() {}
void member_alpha4() {}
void member_beta4() {}
void member_alpha5() {}
void member_beta5() {}
void member_alpha6() {}
void member_beta6() {}
void member_alpha7() {}
void member_beta7() {}
void member_alpha8() {}
void member_beta8() {}
void member_alpha9() {}
void member_beta9() {}
struct DefaultCallbacks {
    Callback cb0 = choice ? member_alpha0 : member_beta0;
    Callback cb1 = choice ? member_alpha1 : member_beta1;
    Callback cb2 = choice ? member_alpha2 : member_beta2;
    Callback cb3 = choice ? member_alpha3 : member_beta3;
    Callback cb4 = choice ? member_alpha4 : member_beta4;
    Callback cb5 = choice ? member_alpha5 : member_beta5;
    Callback cb6 = choice ? member_alpha6 : member_beta6;
    Callback cb7 = choice ? member_alpha7 : member_beta7;
    Callback cb8 = choice ? member_alpha8 : member_beta8;
    Callback cb9 = choice ? member_alpha9 : member_beta9;
};
struct DefaultOwner {
    DefaultCallbacks callbacks;
    DefaultOwner() : callbacks{} {}
};
void default_callbacks() {
    DefaultOwner first;
    first.callbacks.cb0();
    first.callbacks.cb1();
    first.callbacks.cb2();
    first.callbacks.cb3();
    first.callbacks.cb4();
    first.callbacks.cb5();
    first.callbacks.cb6();
    first.callbacks.cb7();
    first.callbacks.cb8();
    first.callbacks.cb9();
    DefaultOwner second;
    second.callbacks.cb0();
    second.callbacks.cb1();
    second.callbacks.cb2();
    second.callbacks.cb3();
    second.callbacks.cb4();
    second.callbacks.cb5();
    second.callbacks.cb6();
    second.callbacks.cb7();
    second.callbacks.cb8();
    second.callbacks.cb9();
}

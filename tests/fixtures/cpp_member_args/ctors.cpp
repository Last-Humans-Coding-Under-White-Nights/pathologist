// Constructor arguments bind past the implicit `this`: `new T(...)` and
// member initializer lists (#94).
typedef void (*Callback)();

void OnNewVar() {}
void OnNewName() {}
void OnNewStmt() {}
void OnBaseName() {}
void OnBaseVar() {}
void OnMember() {}
void OnBraceMember() {}
void OnBraceBase() {}

class Worker {
public:
    Worker(Callback cb) { cb(); }
};

void MakeWithVar() {
    Callback f = OnNewVar;
    Worker *w = new Worker(f);
    (void)w;
}

void MakeByName() {
    Worker *w = new Worker(OnNewName);
    (void)w;
}

void MakeStatement() { new Worker(OnNewStmt); }

class Base {
public:
    Base(Callback cb) { cb(); }
};

class ByName : public Base {
public:
    ByName() : Base(OnBaseName) {}
};

class ByVar : public Base {
public:
    ByVar(Callback f) : Base(f) {}
};

class BraceBase : public Base {
public:
    BraceBase() : Base{OnBraceBase} {}
};

class Member {
public:
    Member(Callback cb) { cb(); }
};

class Holder {
public:
    Member m_;
    Holder(Callback f) : m_(f) {}
};

class BraceHolder {
public:
    Member m_;
    BraceHolder() : m_{OnBraceMember} {}
};

void Construct() {
    ByName *a = new ByName();
    ByVar *b = new ByVar(OnBaseVar);
    BraceBase *c = new BraceBase();
    Holder *d = new Holder(OnMember);
    BraceHolder *e = new BraceHolder();
    (void)a;
    (void)b;
    (void)c;
    (void)d;
    (void)e;
}

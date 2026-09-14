// A method body can call a method its class defines further down (#96).
typedef void (*Callback)();

class Parser {
public:
    int Parse() { return ReadHeader(); }
    int ReadHeader() { return 1; }
};

class Parser2 {
public:
    int ReadHeader() { return 1; }
    int Parse() { return ReadHeader(); }
};

// Arguments reach the later method's parameters, past its `this`.
void OnLater() {}

class Runner {
public:
    void Run() { Invoke(OnLater); }
    void Invoke(Callback cb) { cb(); }
};

// Overloads defined later are told apart by arity.
class Overloads {
public:
    int Use() { return Get(1) + Get(1, 2); }
    int Get(int a) { return a; }
    int Get(int a, int b) { return a + b; }
};

// Class scope finds the member before a global function of the same name.
void Helper() {}

class Shadow {
public:
    void Go() { Helper(); }
    void Helper() {}
};

// A member defined later in the derived class hides the base one.
class Base {
public:
    void Name() {}
};

class Derived : public Base {
public:
    void Call() { Name(); }
    void Name() {}
};

// Class templates, lambdas in a body, and a member class's own later method.
template <class T>
class Box {
public:
    void Open() { Unpack(); }
    void Unpack() {}
};

class Outer {
public:
    void Spawn() {
        auto job = [this]() { Work(); };
        job();
    }
    class Inner {
    public:
        void Start() { Step(); }
        void Step() {}
    };
    void Work() {}
};

// A parameter named like a later method is the callee, not the method.
class Local {
public:
    void Apply(Callback Later) { Later(); }
    void Later() {}
};

// An overload of the calling body's own name, defined below it, is the
// callee; an overload above it does not stand in for one below.
void OnNoArgs() {}
void OnTwoArgs() {}

class Forward {
public:
    void Process(int x) { Process(); }
    void Process() { OnNoArgs(); }
    void Get(int a) {}
    void Use() { Get(1, 2); }
    void Get(int a, int b) { OnTwoArgs(); }
};

// A body constructing its own class, with the constructors defined below it.
class Built {
public:
    static Built Make() { return Built(); }
    void Copy() {
        Built b(1);
        Built *p = new Built(2);
    }
    Built() {}
    Built(int a) {}
};

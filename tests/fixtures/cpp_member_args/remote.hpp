// A class whose members are defined in another translation unit: callers
// here only see the in-class prototypes.
typedef void (*Callback)();

class Remote {
public:
    void Later(Callback cb);
    static void Shared(Callback cb);
    // Declared only: no definition anywhere in the tree.
    static void Declared(Callback cb);
};

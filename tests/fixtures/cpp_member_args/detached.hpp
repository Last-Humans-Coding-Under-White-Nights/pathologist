// A class whose members are defined in a unit that cannot see this header.
typedef void (*Callback)();

class Detached {
public:
    void Later(Callback cb);
    static void Shared(Callback cb);
    void Reset();
};

// Overloads a caller sees only as in-class prototypes: the prototypes carry
// no parameter variables, so only the arity they declare tells them apart.
typedef void (*Callback)();
enum Mode { MODE_A, MODE_B };

class Session {
public:
    Mode Get();
    int Get(Mode &mode);
    void Run(Callback cb);
    void Run(Callback cb, int times);
    void Log(Callback cb, int level = 0);
    void Log(const char *text);
    void Emit(Callback cb);
    void Emit(Callback cb, ...);
};

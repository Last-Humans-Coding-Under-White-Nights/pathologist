// One program declares a class `ns::Clock`, with a member of the same name
// and arity as the other program's namespace function ...
typedef void (*Callback)();

namespace ns {
class Clock {
public:
    static long Now();
    static void Format(Callback cb);
};
}

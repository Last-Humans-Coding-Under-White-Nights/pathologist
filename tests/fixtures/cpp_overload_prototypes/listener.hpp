typedef void (*Callback)();

class Listener {
public:
    void Fire(const char *name, Callback cb) const;
};

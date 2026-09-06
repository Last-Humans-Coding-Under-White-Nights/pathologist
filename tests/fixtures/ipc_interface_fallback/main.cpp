// Stub with no handler method bodies — OnRemoteRequest switch calls
// inherited interface methods directly on `this`. Only OnRemoteRequest
// has a body; HasNext/GetNext are declared as overrides (no body) in the
// stub class.  This matches the FaultLogQueryResultStub pattern in hiview
// where the stub doesn't define the interface methods.

class IRemoteObject {
public:
    int SendRequest(int code, void *data, void *reply, void *option);
};

IRemoteObject *Remote();

// An unrelated same-named interface in another namespace must not be chosen
// by the fallback merely because it was indexed first.
namespace Other {
class IQueryResult {
public:
    virtual bool HasNext() = 0;
    virtual int GetNext() = 0;
};
}

// Same namespace and matching class/method spelling, but not a base of the
// stub. The fallback must not bridge to it.
class QueryResult {
public:
    virtual bool HasNext() = 0;
    virtual int GetNext() = 0;
};

// Interface: IPC methods (declared, no body — external in symbol table).
class IQueryResult {
public:
    virtual bool HasNext() = 0;
    virtual int GetNext() = 0;
};

// Stub: declares HasNext/GetNext as overrides (no body — external).
// OnRemoteRequest has a body. The switch calls the inherited interface
// methods directly on `this`.
class QueryResultStub : public IQueryResult {
public:
    bool HasNext() override;
    int GetNext() override;
    int OnRemoteRequest(int code, void *data, void *reply, void *option);
};

void HasNextImpl() {}
void GetNextImpl() {}

// Concrete in-tree server: bridge targets should prefer these bodies over
// the bodyless interface declarations above.
class QueryResultService : public QueryResultStub {
public:
    bool HasNext() override {
        HasNextImpl();
        return true;
    }
    int GetNext() override {
        GetNextImpl();
        return 1;
    }
};

// Proxy: methods call SendRequest.
class QueryResultProxy {
public:
    bool HasNext();
    int GetNext();
};

int QueryResultStub::OnRemoteRequest(int code, void *data, void *reply, void *option) {
    switch (code) {
        case 1: return HasNext() ? 1 : 0;
        case 2: return GetNext();
        default: return -1;
    }
}

bool QueryResultProxy::HasNext() {
    IRemoteObject *remote = Remote();
    void *data = 0, *reply = 0, *option = 0;
    remote->SendRequest(1, data, reply, option);
    return false;
}
int QueryResultProxy::GetNext() {
    IRemoteObject *remote = Remote();
    void *data = 0, *reply = 0, *option = 0;
    remote->SendRequest(2, data, reply, option);
    return 0;
}

// Template-wrapper inheritance used by OpenHarmony stubs. The wrapper and
// stub deliberately live in different namespaces: the unqualified IWrapped
// argument resolves where WrappedStub is declared, not in OHOS.
namespace OHOS {
template <typename Interface>
class IRemoteStub : public Interface {};

// An unrelated interface with the same simple name must not be selected.
class IWrapped {
public:
    virtual int Fetch() = 0;
};
}

namespace svc {
class IWrapped {
public:
    virtual int Fetch() = 0;
};

class WrappedStub : public OHOS::IRemoteStub<IWrapped> {
public:
    int OnRemoteRequest(int code, void *data, void *reply, void *option) {
        return Fetch();
    }
};

class WrappedProxy {
public:
    int Fetch();
};

int WrappedProxy::Fetch() {
    IRemoteObject *remote = Remote();
    remote->SendRequest(4, nullptr, nullptr, nullptr);
    return 0;
}
}

// A relative qualified argument also resolves from the stub's declaration
// namespace. The global api::IRelative is deliberately unrelated.
namespace api {
class IRelative {
public:
    virtual int Run() = 0;
};
}

namespace svc {
namespace api {
class IRelative {
public:
    virtual int Run() = 0;
};
}

class RelativeStub : public OHOS::IRemoteStub<api::IRelative> {
public:
    int OnRemoteRequest(int code, void *data, void *reply, void *option) {
        return Run();
    }
};

class RelativeProxy {
public:
    int Run();
};

int RelativeProxy::Run() {
    IRemoteObject *remote = Remote();
    remote->SendRequest(6, nullptr, nullptr, nullptr);
    return 0;
}
}

// A default implementation on the interface is a reachable handler too.
// It must not be discarded just because it is defined on an ancestor rather
// than an override below the stub.
void DefaultRunImpl() {}

class IDefault {
public:
    virtual int Run() {
        DefaultRunImpl();
        return 1;
    }
};

class DefaultStub : public IDefault {
public:
    int OnRemoteRequest(int code, void *data, void *reply, void *option) {
        return Run();
    }
};

class DefaultProxy {
public:
    int Run();
};

int DefaultProxy::Run() {
    IRemoteObject *remote = Remote();
    remote->SendRequest(5, nullptr, nullptr, nullptr);
    return 0;
}

// A suffix-matching class with only a constructor is not sufficient evidence
// of an IPC stub, even if a matching interface declaration exists.
class IConstructorOnly {
public:
    virtual int Ping() = 0;
};

class ConstructorOnlyStub {
public:
    ConstructorOnlyStub() {}
};

class ConstructorOnlyProxy {
public:
    int Ping();
};

int ConstructorOnlyProxy::Ping() {
    IRemoteObject *remote = Remote();
    remote->SendRequest(3, nullptr, nullptr, nullptr);
    return 0;
}

int main() {
    QueryResultProxy p;
    p.HasNext();
    return 0;
}

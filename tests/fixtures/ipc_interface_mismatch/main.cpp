// Proxy/stub pairs whose names pair but whose interfaces may not.
class MessageParcel {};
class IRemoteObject {
public:
    int SendRequest(int code, MessageParcel &data, MessageParcel &reply);
};
template <typename T> class IRemoteProxy;
template <typename T> class IRemoteStub;

struct IFoo {
    virtual int Ping() = 0;
};
struct IBar {
    virtual int Ping() = 0;
};

// Names pair, interfaces differ: two services, no bridge.
class WidgetProxy : public IRemoteProxy<IFoo> {
public:
    int Ping() override
    {
        MessageParcel data, reply;
        return remote_->SendRequest(1, data, reply);
    }

private:
    IRemoteObject *remote_;
};
class WidgetStub : public IRemoteStub<IBar> {
public:
    int Ping() override { return 0; }
};

// A mock of the proxy's interface stays a CHA target.
class WidgetMock : public IFoo {
public:
    int Ping() override { return 1; }
};
int ping(IFoo *f) { return f->Ping(); }

// One interface, namespace-qualified: bridged.
namespace api {
struct IGood {
    virtual int Ping() = 0;
};
class GoodProxy : public IRemoteProxy<IGood> {
public:
    int Ping() override
    {
        MessageParcel data, reply;
        return remote_->SendRequest(2, data, reply);
    }

private:
    IRemoteObject *remote_;
};
class GoodStub : public IRemoteStub<IGood> {
public:
    int Ping() override { return 2; }
};
}

// Interfaces unknown on one side: name-based pairing as before.
class LegacyProxy {
public:
    int Ping()
    {
        MessageParcel data, reply;
        return remote_->SendRequest(3, data, reply);
    }

private:
    IRemoteObject *remote_;
};
class LegacyStub {
public:
    int Ping() { return 3; }
};

// The stub's interface comes through a middle class: still IBar, no bridge.
class RelayStubBase : public IRemoteStub<IBar> {};
class RelayProxy : public IRemoteProxy<IFoo> {
public:
    int Ping() override
    {
        MessageParcel data, reply;
        return remote_->SendRequest(4, data, reply);
    }

private:
    IRemoteObject *remote_;
};
class RelayStub : public RelayStubBase {
public:
    int Ping() override { return 4; }
};

// A proxy of a derived interface serves its base's stub: bridged.
struct IFooEx : IFoo {
    virtual int Extra() = 0;
};
class ExtProxy : public IRemoteProxy<IFooEx> {
public:
    int Ping() override
    {
        MessageParcel data, reply;
        return remote_->SendRequest(5, data, reply);
    }
    int Extra() override { return 0; }

private:
    IRemoteObject *remote_;
};
class ExtStub : public IRemoteStub<IFoo> {
public:
    int Ping() override { return 5; }
};

// The stub's interface is known only by a method defined out of class: not
// evidence enough to drop the bridge.
class OrphanProxy : public IRemoteProxy<IFoo> {
public:
    int Ping() override
    {
        MessageParcel data, reply;
        return remote_->SendRequest(6, data, reply);
    }

private:
    IRemoteObject *remote_;
};
class OrphanStub : public IRemoteStub<IOrphan> {
public:
    int Ping() { return 6; }
};
int IOrphan::Ping() { return 0; }

// A proxy base template's parameter is not an interface, whatever else in
// the tree shares its name.
struct Interface {};
template <class Interface> class ProxyBase : public IRemoteProxy<Interface> {};
class TplProxy : public ProxyBase<IFoo> {
public:
    int Ping() override
    {
        MessageParcel data, reply;
        return remote_->SendRequest(7, data, reply);
    }

private:
    IRemoteObject *remote_;
};
class TplStub : public IRemoteStub<IFoo> {
public:
    int Ping() override { return 7; }
};

// A proxy naming its interface through an alias serves the aliased class,
// whatever unrelated class shares the alias's bare name.
struct IAliased {};
namespace impl {
struct IReal {
    virtual int Ping() = 0;
};
}
namespace alias_ns {
using IAliased = impl::IReal;
class AliasProxy : public IRemoteProxy<IAliased> {
public:
    int Ping() override
    {
        MessageParcel data, reply;
        return remote_->SendRequest(8, data, reply);
    }

private:
    IRemoteObject *remote_;
};
class AliasStub : public IRemoteStub<impl::IReal> {
public:
    int Ping() override { return 8; }
};
}

// Two declared readings of one spelling: the index cannot tell which the
// proxy's unit sees, so the bridge stays.
namespace sh {
struct IShadow {
    virtual int Ping() = 0;
};
namespace cam {
struct IShadow {};
class ShadowProxy : public IRemoteProxy<IShadow> {
public:
    int Ping() override
    {
        MessageParcel data, reply;
        return remote_->SendRequest(9, data, reply);
    }

private:
    IRemoteObject *remote_;
};
class ShadowStub : public IRemoteStub<sh::IShadow> {
public:
    int Ping() override { return 9; }
};
}
}

// A stub base template's parameter is no ancestor for the fallback either.
struct Iface2 {
    int Ping();
};
template <class Iface2> class StubBase2 : public IRemoteStub<Iface2> {};
class DepProxy : public IRemoteProxy<IFoo> {
public:
    int Ping() override
    {
        MessageParcel data, reply;
        return remote_->SendRequest(10, data, reply);
    }

private:
    IRemoteObject *remote_;
};
class DepStub : public StubBase2<IFoo> {
public:
    int OnRemoteRequest();
};

// A proxy naming its interface through a using-directive may mean the
// imported class, whatever unrelated class shares its bare name: bridged.
struct IUsed {};
namespace used_api {
struct IUsed {
    virtual int Ping() = 0;
};
}
namespace client {
using namespace used_api;
class UsedProxy : public IRemoteProxy<IUsed> {
public:
    int Ping() override
    {
        MessageParcel data, reply;
        return remote_->SendRequest(11, data, reply);
    }

private:
    IRemoteObject *remote_;
};
class UsedStub : public IRemoteStub<used_api::IUsed> {
public:
    int Ping() override { return 11; }
};
}

// The interface recovered from a stub's base is walked like any class: its
// own `IRemoteStub<I>` spelling reaches the class declaring the method.
struct IDeep {
    virtual int Ping() = 0;
};
class IMid : public IRemoteStub<IDeep> {};
class DeepProxy : public IRemoteProxy<IMid> {
public:
    int Ping()
    {
        MessageParcel data, reply;
        return remote_->SendRequest(12, data, reply);
    }

private:
    IRemoteObject *remote_;
};
class DeepStub : public IRemoteStub<IMid> {
public:
    int OnRemoteRequest() { return 0; }
};

// An explicitly global interface names that class only: no bridge to a
// stub of another namespace's namesake.
struct IGlob {
    virtual int Ping() = 0;
};
namespace gapi {
struct IGlob {
    virtual int Ping() = 0;
};
}
class GlobProxy : public IRemoteProxy<::IGlob> {
public:
    int Ping() override
    {
        MessageParcel data, reply;
        return remote_->SendRequest(13, data, reply);
    }

private:
    IRemoteObject *remote_;
};
class GlobStub : public IRemoteStub<gapi::IGlob> {
public:
    int Ping() override { return 13; }
};

// A bare spelling may name an alias a using-directive imports: it reads as
// the aliased class, whatever unrelated class shares the alias's name.
struct IAl {};
namespace real_api {
struct IRealAl {
    virtual int Ping() = 0;
};
using IAl = IRealAl;
}
namespace client2 {
using namespace real_api;
class AlProxy : public IRemoteProxy<IAl> {
public:
    int Ping() override
    {
        MessageParcel data, reply;
        return remote_->SendRequest(14, data, reply);
    }

private:
    IRemoteObject *remote_;
};
class AlStub : public IRemoteStub<real_api::IRealAl> {
public:
    int Ping() override { return 14; }
};
}

"""Append-only frozen-registry checks, also callable by release CI."""
def check(old,new):
    labels = dict(new.get("labels", []))
    if any(labels.get(name) != value for name, value in old.get("labels", [])):
        raise ValueError("changed frozen wire label")
    def indexed(rows,key):
        result={row[key]:row for row in rows}
        if len(result)!=len(rows):
            raise ValueError("duplicate registry allocation")
        return result
    def fields(before,after):
        a=indexed(before["fields"],"key")
        b=indexed(after["fields"],"key")
        for key,field in a.items():
            if b.get(key)!=field:
                raise ValueError("changed/removed frozen field: "+before["name"]+":"+str(key))
        for key,field in b.items():
            if key not in a and (key<=max(a,default=0) or field["absence"]=="required"):
                raise ValueError("new field needs an appended key and an absence default")
    limits=dict(new["limits"])
    if any(limits.get(name)!=value for name,value in old["limits"]):
        raise ValueError("changed frozen limit")
    messages=indexed(old["messages"],"id")
    future=indexed(new["messages"],"id")
    old_caps=set(old["capabilities"])
    for key,entry in messages.items():
        successor=future.get(key)
        if successor is None:
            raise ValueError("removed frozen message")
        for field in ["name","routes","capabilities","pause"]:
            if entry[field]!=successor[field]:
                raise ValueError("changed frozen message contract")
        fields(entry["schema"],successor["schema"])
    for table, live, key in [("reserved_messages", future, "id"), ("reserved_streams", indexed(new["streams"], "kind"), "kind")]:
        before = dict(old.get(table, []))
        after = dict(new.get(table, []))
        if any(after.get(number) != name for number, name in before.items()):
            raise ValueError("changed reserved allocation")
        if any(number in live for number in before):
            raise ValueError("reused reserved allocation")
    if not set(old.get("reserved_capabilities", [])).issubset(new.get("reserved_capabilities", [])):
        raise ValueError("removed capability reservation")
    if set(new.get("reserved_capabilities", [])).intersection(new["capabilities"]):
        raise ValueError("reserved capability is implemented")
    for key,entry in future.items():
        if key in old["tombstones"]:
            raise ValueError("reused tombstoned message")
        if key not in messages and not (set(entry["capabilities"])-old_caps):
            raise ValueError("new message requires a new capability")
    schemas=indexed(new["schemas"],"name")
    for schema in old["schemas"]:
        if schema["name"] not in schemas:
            raise ValueError("removed nested schema")
        fields(schema,schemas[schema["name"]])
    enums=dict(new["enums"])
    for name,variants in old["enums"]:
        current=dict(enums.get(name,[]))
        if any(current.get(key)!=value for key,value in variants):
            raise ValueError("changed frozen enum id")
    streams=indexed(new["streams"],"kind")
    if any(streams.get(s["kind"])!=s for s in old["streams"]):
        raise ValueError("changed frozen stream")
    for key in ["capabilities","baseline","tombstones","diagnostic_events"]:
        if not set(old[key]).issubset(set(new[key])):
            raise ValueError("removed frozen registry allocation")
    for key in ["reserved_fields","errors"]:
        if any(item not in new[key] for item in old[key]):
            raise ValueError("changed frozen reserved field or error")
    for name,ids in old["reserved_fields"]:
        if name in schemas and set(ids).intersection(field["key"] for field in schemas[name]["fields"]):
            raise ValueError("reused reserved field")

    kinds=dict(new.get("register_kinds",[]))
    if any(kinds.get(key)!=name for key,name in old.get("register_kinds",[])):
        raise ValueError("changed frozen register allocation")
    if any(key in kinds for key in old.get("reserved_registers",[])):
        raise ValueError("reused retired register")
    if not set(old.get("reserved_registers",[])).issubset(new.get("reserved_registers",[])):
        raise ValueError("removed retired register reservation")
    reserved=dict(new.get("reserved_enum_values",[]))
    for name,ids in old.get("reserved_enum_values",[]):
        if not set(ids).issubset(reserved.get(name,[])) or set(ids).intersection(dict(enums.get(name,[]))):
            raise ValueError("reused or removed reserved enum value")
    units=dict(new.get("units",[]))
    if any(units.get(name)!=unit for name,unit in old.get("units",[])):
        raise ValueError("changed frozen wire unit")

    current={row[0]:row for row in new["open_enums"]}
    for name,kind,variants in old["open_enums"]:
        if name not in current or current[name][1]!=kind:
            raise ValueError("changed open enum domain")
        bodies=dict(current[name][2])
        if any(key not in bodies or bodies[key]!=body for key,body in variants):
            raise ValueError("changed open enum body allocation")

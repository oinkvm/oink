#!/usr/bin/env python3
"""Run HEAD <-> an independently built, hash-pinned v1 codec peer.

Default mode is a release gate: a development baseline or HEAD reused as baseline
is an error. --development-smoke permits a nonfrozen archived candidate baseline;
it is explicit evidence of codec-only testing and cannot satisfy release freeze.
Authenticated network and native acceptance are separate runtime tests.
"""
import argparse
import hashlib
import json
import pathlib
import subprocess
from schema_compatibility import check as check_schema


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def identity(path):
    return json.loads(subprocess.check_output([str(path), "--identity"], text=True))


def run(peer, requests):
    payload="".join(json.dumps(request, separators=(",", ":"))+"\n" for request in requests)
    result=subprocess.run([str(peer)],input=payload,text=True,capture_output=True,check=True,timeout=120)
    replies=[json.loads(line) for line in result.stdout.splitlines()]
    if len(replies)!=len(requests):
        raise RuntimeError("peer reply count mismatch")
    return replies


def negotiation_matrix(older, newer):
    cases = [
        ({"minor": 0}, {"minor": 1}, 0, None),
        ({"minor": 1}, {"minor": 1}, 1, None),
        ({}, {"also_major2": True}, 0, None),
        ({}, {"major": 2}, None, 8),
        ({}, {"omit": "oink.clipboard/1"}, None, 9),
        ({"capabilities": ["vendor.future/1"]},
         {"capabilities": ["vendor.future/1", "oink.aim/1"], "future": True}, 0, None),
        ({"capabilities": ["oink.aim/1"]},
         {"capabilities": ["oink.aim/1"], "future": True}, 0, None),
    ]
    count = 0
    for initiator, responder in [(older, newer), (newer, older)]:
        for a, b, minor, error in cases:
            first = run(initiator, [dict(a, kind="offer")])[0]
            second = run(responder, [dict(b, kind="offer", responder=True)])[0]
            if not first.get("ok") or not second.get("ok"):
                raise RuntimeError("peer could not produce negotiation offer")
            request = {"kind": "negotiate", "hex": first["hex"], "remote_hex": second["hex"]}
            replies = [run(peer, [request])[0] for peer in [older, newer]]
            if replies[0] != replies[1]:
                raise RuntimeError("independent peers selected different agreements")
            result = replies[0]
            if error is not None:
                if result.get("ok") or result.get("code") != error:
                    raise RuntimeError("negotiation failure category mismatch")
            elif not result.get("ok") or result["major"] != 1 or result["minor"] != minor:
                raise RuntimeError("negotiation version selection mismatch")
            else:
                expected_aim = "oink.aim/1" in a.get("capabilities", []) and "oink.aim/1" in b.get("capabilities", [])
                if ("oink.aim/1" in result["enabled"]) != expected_aim or "vendor.future/1" in result["enabled"]:
                    raise RuntimeError("negotiation enabled an unimplemented or nonshared capability")
                for peer in [older, newer]:
                    retained = run(peer, [{"kind": "dto", "schema": "Negotiated", "hex": result["hex"]}])[0]
                    if not retained.get("ok") or retained["hex"] != result["hex"]:
                        raise RuntimeError("negotiated agreement changed during peer roundtrip")
            count += 1
    return count


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--head",type=pathlib.Path,required=True)
    parser.add_argument("--baseline",type=pathlib.Path,required=True)
    parser.add_argument("--baseline-sha256",required=True)
    parser.add_argument("--baseline-revision",required=True)
    parser.add_argument("--development-smoke",action="store_true")
    args=parser.parse_args()
    if args.head.resolve()==args.baseline.resolve() or digest(args.head)==digest(args.baseline):
        raise RuntimeError("baseline must be independently built, not the HEAD executable")
    if digest(args.baseline)!=args.baseline_sha256:
        raise RuntimeError("baseline artifact hash mismatch")
    old,new=identity(args.baseline),identity(args.head)
    if old["revision"]!=args.baseline_revision or old["revision"]==new["revision"] or old["revision"]=="development":
        raise RuntimeError("baseline source provenance is missing or equals HEAD")
    if not args.development_smoke and not old["frozen"]:
        raise RuntimeError("release compatibility requires the archived frozen v1 baseline")
    old_schema = json.loads(subprocess.check_output([str(args.baseline),"--schema"],text=True))
    new_schema = json.loads(subprocess.check_output([str(args.head),"--schema"],text=True))
    check_schema(old_schema, new_schema)
    fixtures=pathlib.Path(__file__).parent/"fixtures"/"v1"
    manifest=json.loads((fixtures/"manifest.json").read_text())
    requests=[]
    extensions=[]
    old_messages={entry["id"] for entry in old_schema["messages"]}
    old_dtos={entry["name"] for entry in old_schema["schemas"]}
    for item in manifest["fixtures"]:
        request={"kind":item["kind"],"hex":(fixtures/item["file"]).read_text().strip()}
        if "schema" in item:
            request["schema"]=item["schema"]
        if (item["kind"] == "frame" and item["type_id"] not in old_messages) or (item["kind"] == "dto" and item["schema"] not in old_dtos):
            extensions.append(request)
        else:
            requests.append(request)
    # Capability-only allocations must decode on the new endpoint and must not
    # be sent to an old endpoint. Verify the old decoder rejects their frames;
    # all shared/frozen fixtures still traverse both actual codec binaries.
    replies=run(args.head, extensions)
    if any(not reply.get("ok") or reply["hex"] != request["hex"] for request,reply in zip(extensions,replies)):
        raise RuntimeError("new capability fixture failed")
    extension_frames=[r for r in extensions if r["kind"] == "frame"]
    if any(reply.get("ok") for reply in run(args.baseline, extension_frames)):
        raise RuntimeError("old endpoint unexpectedly recognizes a new message allocation")
    for sender,receiver in [(args.head,args.baseline),(args.baseline,args.head)]:
        encoded=run(sender,requests)
        if any(not reply.get("ok") for reply in encoded):
            raise RuntimeError("sender rejected golden fixture")
        forwarded=[dict(request,hex=reply["hex"]) for request,reply in zip(requests,encoded)]
        decoded=run(receiver,forwarded)
        for request,reply in zip(requests,decoded):
            if not reply.get("ok") or reply["hex"]!=request["hex"]:
                raise RuntimeError("golden/unknown-field preservation regression")
    malformed=[{"kind":"frame","hex":value} for value in [
        "341201000000a0", "00f001000000a0", "050103000000a101", "050103000000a10107ff",
        "050105000000a201070108", "050104000000a1011807"]]
    expected=[6,6,4,4,4,4]
    for peer in [args.head,args.baseline]:
        actual=run(peer,malformed)
        if [item.get("code") for item in actual]!=expected:
            raise RuntimeError("typed failure regression")
    # Exercise a real edit by each independently built codec, then re-save it
    # through the other codec. The writer/version/defaults and unknown envelope
    # are all part of the retained digest, so both outputs must be identical.
    register=(fixtures/"dto-Register.hex").read_text().strip()
    edit={"kind":"register-edit","hex":register}
    a=run(args.head,[edit])[0]
    b=run(args.baseline,[edit])[0]
    if not a.get("ok") or a!=b:
        raise RuntimeError("old/new register edit mismatch")
    for peer in [args.head,args.baseline]:
        reply=run(peer,[{"kind":"dto","schema":"Register","hex":a["hex"]}])[0]
        if reply!=a:
            raise RuntimeError("edited unknown register data lost on re-save")
    negotiations = negotiation_matrix(args.baseline, args.head)
    print(json.dumps({"mode":"development-smoke" if args.development_smoke else "frozen-release-gate",
        "baseline_revision":old["revision"],"baseline_sha256":args.baseline_sha256,
        "head_schema_sha256":new["schema_sha256"],"vectors":len(requests),"capability_extension_vectors":len(extensions),"old_rejected_extension_frames":len(extension_frames),"directions":2,"malformed":len(expected),"negotiations":negotiations},sort_keys=True))

if __name__=="__main__":
    main()

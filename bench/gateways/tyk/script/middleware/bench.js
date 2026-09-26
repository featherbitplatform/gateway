var benchScript = new TykJS.TykMiddleware.NewMiddleware({});

benchScript.NewProcessRequest(function (request, session) {
  var values = request.Headers["X-Bench-In"];
  var v = values && values.length ? values[0] : "";
  request.SetHeaders["X-Bench-Script"] = v.toUpperCase() + "-" + v.length;
  return benchScript.ReturnData(request, {});
});

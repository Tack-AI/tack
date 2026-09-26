cat > index.html <<'HTML'
<!DOCTYPE html>
<html>
<head>
  <link rel="stylesheet" href="style.css">
  <title>Pi Eval</title>
</head>
<body>
  <h1>Pi Eval</h1>
  <p class="intro">A tiny page.</p>
  <ul id="langs">
    <li>Rust</li>
    <li>Python</li>
    <li>Shell</li>
  </ul>
</body>
</html>
HTML
cat > style.css <<'CSS'
h1 {
  font-size: 2em;
}

.intro {
  color: red;
}
CSS
